//! The composer's model selector: one button that reads `<Provider> · <model>`, and a
//! two-column menu that chooses the provider on the left and, from that provider's own
//! catalog, the model on the right. Both are fixed once the session has sent.

use gpui::{
    Anchor, App, Div, ElementId, FontWeight, IntoElement, ParentElement, Pixels, SharedString,
    Stateful, Styled, Window, div, prelude::*, px,
};
use keystone_agent::{Catalog, Commands, ModelInfo};
use keystone_core::{Account, AccountId, Provider, WorkingDirectory};
use keystone_ui::prelude::*;
use std::collections::HashMap;
use std::rc::Rc;

/// The narrowest the button gets: its provider icon, still a target to hover and press.
pub const MIN_WIDTH: Pixels = px(28.);

/// The space the menu leaves around itself and inside its card: the window margin it
/// snaps to on both sides, and the card's padding and border.
const MENU_INSET: Pixels = px(26.);

/// What is known about one catalog in this run of the app.
pub enum CatalogState {
    Loading,
    Ready(Vec<ModelInfo>),
    Failed(String),
}

/// A provider account's catalog as it answers in one conversation folder. Accounts
/// may offer different models, so no catalog is shared between them.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct CatalogKey {
    pub provider: Provider,
    /// `None` is the provider's default account.
    pub account: Option<AccountId>,
    pub directory: WorkingDirectory,
}

/// Each catalog, asked once per app run and kept until Retry, with the commands the
/// same request lists.
#[derive(Default)]
pub struct Catalogs {
    states: HashMap<CatalogKey, CatalogState>,
    asked: HashMap<(Provider, Option<AccountId>, u64), WorkingDirectory>,
    commands: HashMap<CatalogKey, Option<Commands>>,
    commands_asked: HashMap<(Provider, Option<AccountId>, u64), WorkingDirectory>,
}

impl Catalogs {
    pub fn choices(&self, key: &CatalogKey) -> Choices<'_> {
        match self.states.get(key) {
            None | Some(CatalogState::Loading) => Choices::Loading,
            Some(CatalogState::Ready(models)) => Choices::Ready(models),
            Some(CatalogState::Failed(message)) => Choices::Failed(message),
        }
    }

    /// The commands listed for `key`: `None` while they load.
    pub fn commands(&self, key: &CatalogKey) -> Option<&Commands> {
        self.commands.get(key)?.as_ref()
    }

    pub fn ask(&mut self, key: CatalogKey, session: u64) -> bool {
        if self.states.contains_key(&key) {
            return false;
        }
        let asker = (key.provider, key.account.clone(), session);
        self.asked.insert(asker.clone(), key.directory.clone());
        self.commands_asked.insert(asker, key.directory.clone());
        self.commands.insert(key.clone(), None);
        self.states.insert(key, CatalogState::Loading);
        true
    }

    pub fn apply_commands(
        &mut self,
        provider: Provider,
        account: Option<AccountId>,
        session: u64,
        commands: Commands,
    ) {
        let Some(directory) = self
            .commands_asked
            .remove(&(provider, account.clone(), session))
        else {
            return;
        };
        let key = CatalogKey {
            provider,
            account,
            directory,
        };
        self.commands.insert(key, Some(commands));
    }

    pub fn apply(
        &mut self,
        provider: Provider,
        account: Option<AccountId>,
        session: u64,
        catalog: Catalog,
    ) {
        let Some(directory) = self.asked.remove(&(provider, account.clone(), session)) else {
            return;
        };
        let state = match catalog {
            // What the default resolves to is not shown: rows are names only.
            Catalog::Ready { models, .. } => CatalogState::Ready(models),
            Catalog::Failed(message) => {
                // Claude lists commands only in a ready catalog's answer.
                if provider == Provider::Claude {
                    let failed = Commands::Failed(message.clone());
                    self.apply_commands(provider, account.clone(), session, failed);
                }
                CatalogState::Failed(message)
            }
        };
        self.states.insert(
            CatalogKey {
                provider,
                account,
                directory,
            },
            state,
        );
    }

    pub fn clear(&mut self, key: &CatalogKey) {
        self.states.remove(key);
        self.commands.remove(key);
    }

    /// Drops everything asked of a removed account, so an account added later under
    /// the same ID asks again.
    pub fn forget_account(&mut self, account: &AccountId) {
        let gone = |id: &Option<AccountId>| id.as_ref() == Some(account);
        self.states.retain(|key, _| !gone(&key.account));
        self.asked.retain(|(_, id, _), _| !gone(id));
        self.commands.retain(|key, _| !gone(&key.account));
        self.commands_asked.retain(|(_, id, _), _| !gone(id));
    }
}

/// What the model column offers one session.
#[derive(Clone, Copy)]
pub enum Choices<'a> {
    /// The session has no folder this version can run the provider in, so nothing is
    /// asked.
    Unavailable(&'static str),
    Loading,
    Ready(&'a [ModelInfo]),
    Failed(&'a str),
}

impl Choices<'_> {
    /// The label the catalog gives a model, asked for by what it is launched as or by
    /// what the provider reports it runs as, or the ID itself when the catalog does not
    /// know it.
    fn label(self, model: &str) -> String {
        match self {
            Choices::Ready(models) => models
                .iter()
                .find(|m| m.model == model || m.resolved.as_deref() == Some(model))
                .map_or(model, |m| m.label.as_str())
                .to_owned(),
            _ => model.to_owned(),
        }
    }
}

/// The session the selector shows.
#[derive(Clone, Copy)]
pub struct Selection<'a> {
    pub provider: Provider,
    /// The session's account; `None` is the provider's default.
    pub account: Option<&'a AccountId>,
    /// Every account the user added, of either provider.
    pub accounts: &'a [Account],
    /// The provider-native model the session asked for; `None` is the provider's default.
    pub model: Option<&'a str>,
    /// The model the provider has reported for this session, in this run of the app.
    pub reported_model: Option<&'a str>,
    /// Whether the session has sent, which fixes its provider and model.
    pub started: bool,
    pub choices: Choices<'a>,
}

type Listener<T> = Rc<dyn Fn(&T, &mut Window, &mut App) + 'static>;

/// What the selector does for the user.
pub struct Handlers {
    pub open_change: Listener<bool>,
    pub choose_provider: Listener<Provider>,
    /// An account of the named provider; `None` chooses its default account.
    pub choose_account: Listener<(Provider, Option<AccountId>)>,
    /// A model from the named provider's catalog; `None` chooses that provider's
    /// default.
    pub choose_model: Listener<(Provider, Option<String>)>,
    pub retry: Listener<Provider>,
}

pub fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "Claude Code",
        Provider::Codex => "Codex",
    }
}

pub fn provider_color(provider: Provider, cx: &App) -> Color {
    let colors = &cx.theme().colors;
    Color::Custom(match provider {
        Provider::Claude => colors.provider_claude,
        Provider::Codex => colors.provider_codex,
    })
}

pub fn provider_icon(provider: Provider) -> IconName {
    match provider {
        Provider::Claude => IconName::Claude,
        Provider::Codex => IconName::Codex,
    }
}

pub fn provider_icon_color(provider: Provider, cx: &App) -> Color {
    match provider {
        Provider::Claude => provider_color(provider, cx),
        Provider::Codex => Color::Default,
    }
}

const FIXED: &str = "Provider, account and model are fixed once a session has sent. Start a new session to choose others.";

/// How a session on `provider` names its account: `None` while the user has added no
/// account for that provider, so a single account is never mentioned.
pub fn account_label(
    provider: Provider,
    account: Option<&AccountId>,
    accounts: &[Account],
) -> Option<String> {
    if !accounts.iter().any(|a| a.provider() == provider) {
        return None;
    }
    Some(match account {
        None => "Default".to_owned(),
        Some(id) => accounts
            .iter()
            .find(|a| a.id() == id)
            .map_or_else(|| id.to_string(), |a| a.label().to_owned()),
    })
}

/// `provider`'s accounts after its default, or none when the user added none, so a
/// single-account provider shows no account at all.
fn accounts_of(provider: Provider, accounts: &[Account]) -> Vec<(Option<&AccountId>, &str)> {
    let added: Vec<_> = accounts
        .iter()
        .filter(|account| account.provider() == provider)
        .map(|account| (Some(account.id()), account.label()))
        .collect();
    if added.is_empty() {
        return added;
    }
    std::iter::once((None, "Default")).chain(added).collect()
}

/// The selector for one session: the button, and the menu while `open`, which fits
/// within `window_width`.
pub fn render(
    selection: Selection,
    open: bool,
    on: Handlers,
    window_width: Pixels,
    cx: &App,
) -> impl IntoElement {
    let (trigger, tooltip) = trigger(selection, open, cx);
    let trigger = trigger.tooltip(Tooltip::text(tooltip).builder());
    if selection.started {
        return trigger.into_any_element();
    }
    let colors = cx.theme().colors;
    let Selection {
        provider,
        account,
        accounts,
        model,
        choices,
        ..
    } = selection;
    let open_change = on.open_change.clone();
    div()
        .min_w(MIN_WIDTH)
        .child(
            PopoverMenu::new("model-menu", trigger)
                .anchor(Anchor::BottomLeft)
                .open(open)
                .on_open_change(move |open, window, cx| open_change(open, window, cx))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .max_w(window_width - MENU_INSET)
                        .child(provider_column(provider, account, accounts, &on, cx))
                        .child(div().flex_none().w_px().ml(px(7.)).bg(colors.border))
                        .child(model_column(provider, model, choices, &on, cx)),
                ),
        )
        .into_any_element()
}

pub fn trigger(selection: Selection, open: bool, cx: &App) -> (Stateful<Div>, String) {
    let Selection {
        provider,
        account,
        accounts,
        model,
        reported_model,
        started,
        choices,
    } = selection;
    let colors = cx.theme().colors;
    let name = match account_label(provider, account, accounts) {
        Some(label) => format!("{} · {label}", provider_name(provider)),
        None => provider_name(provider).to_owned(),
    };
    let requested = model.map_or_else(
        || "Provider default".to_owned(),
        |model| choices.label(model),
    );
    // A requested model is shown as running only once the provider says so: until
    // then the label is muted and the tooltip says what was asked for.
    let (shown, color, tooltip) = match (started, reported_model) {
        (true, Some(reported)) => {
            let shown = choices.label(reported);
            let tooltip = format!("{name} · {shown}. {FIXED}");
            (shown, Color::Default, tooltip)
        }
        (true, None) => {
            let tooltip = format!(
                "Requested {name} · {requested}. {name} has not reported its model yet. {FIXED}"
            );
            (requested, Color::Muted, tooltip)
        }
        (false, _) => {
            let tooltip = format!(
                "{name} · {requested}. Choose the provider, account and model for this session"
            );
            (requested, Color::Default, tooltip)
        }
    };
    let trigger = h_flex()
        .id("model-selector")
        .automation_id("model-selector")
        .min_w(MIN_WIDTH)
        .h(cx.theme().density.control(px(28.)))
        .px(px(6.))
        .gap(px(7.))
        .rounded(radius::CONTROL)
        .child(
            Icon::new(provider_icon(provider))
                .size(px(16.))
                .color(provider_icon_color(provider, cx)),
        )
        .child(
            Label::new(format!("{name} · {shown}"))
                .size(TextSize::Ui)
                .weight(FontWeight::MEDIUM)
                .color(color)
                .truncate(),
        );
    if started {
        return (trigger, tooltip);
    }
    let hover = colors.element_hover;
    let trigger = trigger
        .cursor_pointer()
        .when(open, |d| d.bg(colors.selection_fill))
        .when(!open, |d| d.hover(move |s| s.bg(hover)))
        .child(
            Icon::new(IconName::Chevron)
                .size(px(10.))
                .color(Color::Muted),
        );
    (trigger, tooltip)
}

/// Each provider, followed by its accounts when the user added any.
fn provider_column(
    current: Provider,
    account: Option<&AccountId>,
    accounts: &[Account],
    on: &Handlers,
    cx: &App,
) -> impl IntoElement {
    v_flex()
        .flex_none()
        .w(px(152.))
        .child(MenuHeader::new("Provider"))
        .child(
            v_flex()
                .id("provider-list")
                .debug_selector(|| "provider-list".into())
                .max_h(px(360.))
                .overflow_y_scroll()
                .children([Provider::Claude, Provider::Codex].map(|provider| {
                    let choose = on.choose_provider.clone();
                    let row = choice_row(
                        ("provider", provider as usize),
                        format!("provider-{}", slug(provider)),
                        Some(
                            Icon::new(provider_icon(provider))
                                .size(px(16.))
                                .color(provider_icon_color(provider, cx)),
                        ),
                        provider_name(provider),
                        provider == current,
                        true,
                        cx,
                    )
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        choose(&provider, window, cx)
                    });
                    let rows = accounts_of(provider, accounts).into_iter().enumerate().map(
                        |(at, (id, label))| {
                            let choose = on.choose_account.clone();
                            let chosen = (provider, id.cloned());
                            let selector = match id {
                                Some(id) => format!("account-{id}"),
                                None => format!("account-{}-default", slug(provider)),
                            };
                            div().pl(px(24.)).child(
                                choice_row(
                                    ("account", provider as usize * 1000 + at),
                                    selector,
                                    None,
                                    label.to_owned(),
                                    provider == current && id == account,
                                    false,
                                    cx,
                                )
                                .on_click(move |_, window, cx| {
                                    cx.stop_propagation();
                                    choose(&chosen, window, cx)
                                }),
                            )
                        },
                    );
                    v_flex().child(row).children(rows)
                })),
        )
}

fn model_column(
    provider: Provider,
    chosen: Option<&str>,
    choices: Choices,
    on: &Handlers,
    cx: &App,
) -> impl IntoElement {
    let row = |id: ElementId, selector: String, label: SharedString, model: Option<String>| {
        let choose = on.choose_model.clone();
        let selected = chosen == model.as_deref();
        choice_row(id, selector, None, label, selected, false, cx).on_click(move |_, window, cx| {
            cx.stop_propagation();
            choose(&(provider, model.clone()), window, cx)
        })
    };
    let column = v_flex()
        .id("model-column")
        .automation_id("model-column")
        .w(px(282.))
        .min_w_0()
        .pl(px(8.))
        .child(MenuHeader::new("Model"))
        .child(row(
            "model-default".into(),
            "model-default".into(),
            "Provider default".into(),
            None,
        ));
    let column = match choices {
        Choices::Unavailable(reason) => {
            column.child(note("model-unavailable", reason, Color::Muted))
        }
        Choices::Loading => column.child(note("model-loading", "Loading choices…", Color::Muted)),
        Choices::Ready(models) => column.child(
            v_flex()
                .id("model-list")
                .max_h(px(360.))
                .overflow_y_scroll()
                .children(models.iter().enumerate().map(|(index, model)| {
                    row(
                        ("model", index).into(),
                        format!("model-{}", model.model),
                        model.label.clone().into(),
                        Some(model.model.clone()),
                    )
                })),
        ),
        Choices::Failed(message) => {
            let retry = on.retry.clone();
            column
                .child(note("model-failed", message.to_owned(), Color::Danger))
                .child(
                    choice_row(
                        "model-retry",
                        "model-retry".into(),
                        None,
                        "Retry",
                        false,
                        false,
                        cx,
                    )
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        retry(&provider, window, cx)
                    }),
                )
        }
    };
    column.child(
        div()
            .id("model-footer")
            .automation_id("model-footer")
            .px(px(8.))
            .pt(px(6.))
            .pb(px(4.))
            .child(
            Label::new(format!(
                "Choices reported by {}. The provider may reject an unavailable model when you send.",
                provider_name(provider)
            ))
            .size(TextSize::Sm)
            .color(Color::Muted),
        ),
    )
}

fn choice_row(
    id: impl Into<ElementId>,
    selector: String,
    icon: Option<Icon>,
    label: impl Into<SharedString>,
    selected: bool,
    edge: bool,
    cx: &App,
) -> Stateful<Div> {
    let theme = cx.theme();
    let colors = theme.colors;
    let hover = colors.element_hover;
    let edge = selected && edge;
    let color = if selected {
        Color::Custom(colors.selection_edge)
    } else {
        Color::Default
    };
    let ring = px(if selected { 1. } else { 0. });
    h_flex()
        .id(id)
        .automation_id(selector)
        .flex_shrink_0()
        .h(theme.density.control(px(36.)))
        .pl(px(8.) - ring - px(if edge { 2. } else { 0. }))
        .pr(px(8.) - ring)
        .rounded(radius::MENU_ROW)
        .cursor_pointer()
        .when(selected, |d| {
            d.bg(colors.selection_fill)
                .border_1()
                .border_color(colors.selection_ring)
        })
        .when(!selected, |d| d.hover(move |s| s.bg(hover)))
        .child(
            h_flex()
                .flex_1()
                .h_full()
                .min_w_0()
                .gap_2()
                .when(edge, |d| {
                    d.rounded(radius::MENU_ROW - ring)
                        .border_l_2()
                        .border_color(colors.selection_edge)
                })
                .children(icon)
                .child(
                    div().flex_1().min_w_0().child(
                        Label::new(label)
                            .size(TextSize::Base)
                            .color(color)
                            .truncate(),
                    ),
                )
                .child(
                    div()
                        .w(px(16.))
                        .flex_shrink_0()
                        .flex()
                        .justify_end()
                        .when(selected, |d| {
                            d.child(Icon::new(IconName::Check).size(px(14.)).color(color))
                        }),
                ),
        )
}

/// A row of text in the model column that is not a choice.
fn note(id: &'static str, text: impl Into<SharedString>, color: Color) -> impl IntoElement {
    div()
        .id(id)
        .automation_id(id)
        .px(px(8.))
        .py(px(6.))
        .child(Label::new(text).size(TextSize::Base).color(color))
}

fn slug(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
    }
}
