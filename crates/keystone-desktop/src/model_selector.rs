//! The composer's model selector: one button that reads `<Provider> · <model>`, with the
//! effort and speed when chosen, and a menu that chooses the provider on the left and,
//! from that provider's own catalog, the model and then its effort and speed. The
//! provider and account are fixed once the session has sent; the rest can change.

use gpui::{
    Anchor, AnyElement, App, Div, ElementId, IntoElement, ParentElement, Pixels, ScrollHandle,
    SharedString, Size, Stateful, Styled, Window, div, point, prelude::*, px,
};
use keystone_agent::{Catalog, Commands, ModelInfo};
use keystone_core::{Account, AccountId, MachineId, ModelChoice, Provider, WorkingDirectory};
use keystone_ui::prelude::*;
use std::collections::HashMap;
use std::rc::Rc;

gpui::actions!(model_selector, [NextEffort, ToggleFastMode, NextModel]);

/// The narrowest the button gets: its provider icon, still a target to hover and press.
pub const MIN_WIDTH: Pixels = px(28.);

/// The space the menu leaves around itself and inside its card: the window margin it
/// snaps to on both sides, and the card's padding and border.
const MENU_INSET: Pixels = px(26.);

/// The rows of height the model choices keep whatever their catalog holds, so a
/// catalog arriving, failing or switched to while the menu is open moves no row of the
/// menu, which opens upward. The half row shows a longer catalog scrolls. A short window
/// keeps at most a third of its height for them, and for the providers and accounts.
const CHOICE_ROWS: f32 = 6.5;

/// The rows of height the effort and speed column may take: two titles, a Default and
/// six levels, and a Default and two speeds.
const OPTION_ROWS: f32 = 13.;

/// What is known about one catalog in this run of the app.
pub enum CatalogState {
    Loading,
    /// The models, and the model ID the provider's default resolves to, when it says.
    Ready(Vec<ModelInfo>, Option<String>),
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
            Some(CatalogState::Ready(models, _)) => Choices::Ready(models),
            Some(CatalogState::Failed(message)) => Choices::Failed(message),
        }
    }

    /// The model ID the provider's default resolves to in `key`'s catalog, when known.
    pub fn default_model(&self, key: &CatalogKey) -> Option<&str> {
        match self.states.get(key) {
            Some(CatalogState::Ready(_, default)) => default.as_deref(),
            _ => None,
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
            // What the default resolves to is not shown, but its efforts and speeds are.
            Catalog::Ready {
                models,
                default_model,
            } => CatalogState::Ready(models, default_model),
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
    pub fn label(self, model: &str) -> String {
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
    /// The machine the session runs on and its label, `None` for this one. Only that
    /// machine's logins are offered, each named with the machine.
    pub machine: Option<(&'a MachineId, &'a str)>,
    /// The provider-native model the session asked for; `None` is the provider's default.
    pub model: Option<&'a str>,
    /// The effort and speed the session asked for, from the model's catalog entry.
    pub effort: Option<&'a str>,
    pub speed: Option<&'a str>,
    /// The model the provider has reported for this session, in this run of the app.
    pub reported_model: Option<&'a str>,
    /// The name of the model the provider last reported before this run, saved with the
    /// session. Only the chip shows it; choices still follow `reported_model`.
    pub last_model_name: Option<&'a str>,
    /// What the provider's default resolves to, when its catalog says.
    pub default_model: Option<&'a str>,
    /// Whether the session has sent, which fixes its provider and account.
    pub started: bool,
    pub choices: Choices<'a>,
}

impl<'a> Selection<'a> {
    pub fn choice(&self) -> ModelChoice {
        ModelChoice {
            model: self.model.map(str::to_owned),
            effort: self.effort.map(str::to_owned),
            speed: self.speed.map(str::to_owned),
        }
    }

    /// The catalog entry the session's effort and speed are chosen from: its model's, or
    /// on the provider's default, the one the provider reported or the default resolves
    /// to. `None` before the catalog arrives, or when no entry is known.
    pub fn entry(&self) -> Option<&'a ModelInfo> {
        let Choices::Ready(models) = self.choices else {
            return None;
        };
        entry(models, self.model, self.reported_model, self.default_model)
    }

    /// The catalog's label for the session's speed, or the value itself.
    fn speed_label(&self, speed: &str) -> String {
        self.entry()
            .and_then(|entry| entry.speeds.iter().find(|s| s.value == speed))
            .map_or(speed, |s| s.label.as_str())
            .to_owned()
    }

    /// Whether `speed` is the entry's standard speed, its first, which the button leaves
    /// out.
    fn standard_speed(&self, speed: &str) -> bool {
        self.entry().and_then(|entry| entry.speeds.first()).map_or(
            speed == keystone_agent::standard_speed(self.provider),
            |first| first.value == speed,
        )
    }

    /// The transcript line for the choice the next turns run with, from the catalog's
    /// labels.
    pub fn note(&self) -> String {
        let mut note = format!(
            "Next turns: {}",
            self.model
                .map_or_else(|| "Provider default".to_owned(), |m| self.choices.label(m))
        );
        if let Some(effort) = self.shown_effort() {
            note += &format!(" · effort {effort}");
        }
        if let Some(speed) = self.shown_speed() {
            note += &format!(" · {}", self.speed_label(speed));
        }
        note
    }

    /// The effort and speed the composer button shows after the model: like the note,
    /// leaving out the standard speed as well.
    pub fn options(&self) -> String {
        let mut options = String::new();
        if let Some(effort) = self.shown_effort() {
            options += &format!(" · {effort}");
        }
        if let Some(speed) = self
            .shown_speed()
            .filter(|speed| !self.standard_speed(speed))
        {
            options += &format!(" · {}", self.speed_label(speed));
        }
        options
    }

    /// The speed to show: none for one the model does not list, such as the standard
    /// speed a model without speeds is sent.
    fn shown_speed(&self) -> Option<&'a str> {
        self.speed.filter(|speed| {
            self.entry()
                .is_none_or(|entry| entry.speeds.iter().any(|s| s.value == *speed))
        })
    }

    /// The effort to show: none for a model that lists no levels, which keeps a chosen
    /// effort it does not use.
    fn shown_effort(&self) -> Option<&'a str> {
        self.effort
            .filter(|_| self.entry().is_none_or(|entry| !entry.efforts.is_empty()))
    }

    /// The choice a keyboard command makes, or why it makes none.
    pub fn step(&self, step: Step) -> Result<ModelChoice, &'static str> {
        let Choices::Ready(models) = self.choices else {
            return Err(NOT_LOADED);
        };
        let choice = self.choice();
        let entry = || {
            self.entry()
                .ok_or("Choose a model to set its effort and speed.")
        };
        Ok(match step {
            Step::NextModel => {
                let at = self
                    .entry()
                    .and_then(|current| models.iter().position(|m| m == current));
                let next = models
                    .get(at.map_or(0, |at| (at + 1) % models.len()))
                    .ok_or("No models to choose from.")?;
                switch_model(self, Some(next.model.clone()), Some(next))
            }
            Step::NextEffort => {
                let levels = &entry()?.efforts;
                if levels.is_empty() {
                    return Err("This model has no effort levels.");
                }
                let at = self.effort.and_then(|e| levels.iter().position(|l| l == e));
                ModelChoice {
                    effort: Some(levels[at.map_or(0, |at| (at + 1) % levels.len())].clone()),
                    ..choice
                }
            }
            // Between the standard speed and the one after it.
            Step::ToggleFast => {
                let [standard, fast, ..] = &entry()?.speeds[..] else {
                    return Err("This model has no fast mode.");
                };
                let next = if self.speed == Some(fast.value.as_str()) {
                    standard
                } else {
                    fast
                };
                ModelChoice {
                    speed: Some(next.value.clone()),
                    ..choice
                }
            }
        })
    }
}

/// A keyboard command that changes the session's choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    NextEffort,
    ToggleFast,
    NextModel,
}

/// Why a keyboard command chose nothing while the catalog is not loaded.
pub const NOT_LOADED: &str = "Model choices are not loaded yet. Try again in a moment.";

const NO_ENTRY: &str = "Choose a model to set its effort and speed";

/// The catalog entry for `model`, or for the provider's default, the one the provider
/// reported or the one the default resolves to.
pub fn entry<'a>(
    models: &'a [ModelInfo],
    model: Option<&str>,
    reported: Option<&str>,
    default_model: Option<&str>,
) -> Option<&'a ModelInfo> {
    match model {
        Some(model) => models.iter().find(|m| m.model == model),
        None => reported
            .and_then(|reported| {
                models
                    .iter()
                    .find(|m| m.model == reported || m.resolved.as_deref() == Some(reported))
            })
            .or_else(|| {
                let default = default_model?;
                models
                    .iter()
                    .find(|m| m.resolved.as_deref() == Some(default))
            }),
    }
}

/// The choice after the session moves to `model`, whose catalog entry is `next`.
///
/// Before the first send, a chosen effort stays when `next` lists it, else becomes its
/// default effort, and a chosen speed stays only when `next` lists it; anything else
/// returns to Default. After the first send a field with a value never returns to
/// Default, since sending nothing would leave the provider on the old value: an
/// unlisted effort becomes the default effort, else the nearest lower level `next`
/// lists, and an unlisted speed becomes the provider's standard speed. A catalog model
/// that lists no efforts keeps the effort, which it does not use (Claude Code launched
/// on Haiku 4.5 with `--effort` sends the API none), and a model the catalog does not
/// know keeps both, since nothing can be checked.
pub fn switch_model(
    selection: &Selection,
    model: Option<String>,
    next: Option<&ModelInfo>,
) -> ModelChoice {
    let choice = selection.choice();
    let started = selection.started;
    let effort = choice.effort.and_then(|effort| match next {
        Some(next) if next.efforts.contains(&effort) => Some(effort),
        Some(next) if next.default_effort.is_some() => next.default_effort.clone(),
        Some(next) if started && next.efforts.is_empty() => Some(effort),
        Some(next) if started => {
            // Levels ranked as the catalog model that lists the most of them ranks
            // them, since the current model may list none.
            let before = match selection.choices {
                Choices::Ready(models) => models
                    .iter()
                    .filter(|m| m.efforts.contains(&effort))
                    .max_by_key(|m| m.efforts.len())
                    .map_or(&[][..], |m| &m.efforts[..]),
                _ => &[][..],
            };
            let below = before
                .iter()
                .position(|level| *level == effort)
                .map_or(&[][..], |at| &before[..at]);
            let lower = below
                .iter()
                .rev()
                .find(|level| next.efforts.contains(level));
            lower.or(next.efforts.first()).cloned()
        }
        Some(_) => None,
        None => started.then_some(effort),
    });
    let speed = choice.speed.and_then(|speed| match next {
        Some(next) if next.speeds.iter().any(|s| s.value == speed) => Some(speed),
        Some(_) if started => Some(keystone_agent::standard_speed(selection.provider).to_owned()),
        None if started => Some(speed),
        _ => None,
    });
    ModelChoice {
        model,
        effort,
        speed,
    }
}

type Listener<T> = Rc<dyn Fn(&T, &mut Window, &mut App) + 'static>;

pub struct OptionListeners {
    pub effort: Listener<Option<String>>,
    pub speed: Listener<Option<String>>,
}

/// What the selector does for the user.
pub struct Handlers {
    pub open_change: Listener<bool>,
    pub choose_provider: Listener<Provider>,
    /// An account of the named provider; `None` chooses its default account.
    pub choose_account: Listener<(Provider, Option<AccountId>)>,
    /// A model from the named provider's catalog; `None` chooses that provider's
    /// default.
    pub choose_model: Listener<(Provider, Option<String>)>,
    /// An effort or a speed value from the model's catalog entry; `None` chooses
    /// Default. Without them the menu offers models only.
    pub choose_options: Option<OptionListeners>,
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

/// How a session on `provider` and `machine` names its account: `None` while the user
/// has added no account for that provider there, so a single account is never
/// mentioned.
pub fn account_label(
    provider: Provider,
    account: Option<&AccountId>,
    accounts: &[Account],
    machine: Option<&MachineId>,
) -> Option<String> {
    if !accounts
        .iter()
        .any(|a| a.provider() == provider && a.machine() == machine)
    {
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

/// `provider`'s accounts on `machine` after its default, or none when the user added
/// none there, so a single-account provider shows no account at all.
fn accounts_of<'a>(
    provider: Provider,
    accounts: &'a [Account],
    machine: Option<&MachineId>,
) -> Vec<(Option<&'a AccountId>, &'a str)> {
    let added: Vec<_> = accounts
        .iter()
        .filter(|account| account.provider() == provider && account.machine() == machine)
        .map(|account| (Some(account.id()), account.label()))
        .collect();
    if added.is_empty() {
        return added;
    }
    std::iter::once((None, "Default")).chain(added).collect()
}

/// The selector for one session: the button, and the menu while `open`, which fits
/// within `viewport`.
pub fn render(
    selection: Selection,
    open: bool,
    on: Handlers,
    viewport: Size<Pixels>,
    cx: &App,
) -> impl IntoElement {
    let (trigger, tooltip) = trigger(selection, open, cx);
    let trigger = trigger.tooltip(Tooltip::text(tooltip).builder());
    let colors = cx.theme().colors;
    let Selection {
        provider,
        account,
        accounts,
        machine,
        started,
        ..
    } = selection;
    let separator = || div().flex_none().w_px().ml(px(7.)).bg(colors.border);
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
                        .max_w(viewport.width - MENU_INSET)
                        // The provider and account are fixed once the session sent.
                        .when(!started, |d| {
                            d.child(provider_column(
                                provider, account, accounts, machine, &on, viewport, cx,
                            ))
                            .child(separator())
                        })
                        .child(model_column(selection, &on, viewport, cx))
                        // Shown in every catalog state, so a catalog arriving resizes
                        // no column.
                        .when_some(on.choose_options.as_ref(), |d, choose| {
                            d.child(separator())
                                .child(options_column(selection, choose, viewport, cx))
                        }),
                ),
        )
        .into_any_element()
}

/// The name of the model a session asks for: the chosen one, or what the provider's
/// default resolves to when its catalog says. A default nobody has named yet stays
/// "Default", never a guess.
pub fn requested_model(
    model: Option<&str>,
    default_model: Option<&str>,
    choices: Choices,
) -> String {
    match (model, default_model, choices) {
        (Some(model), ..) => choices.label(model),
        (None, Some(default), Choices::Ready(_)) => choices.label(default),
        _ => "Default".to_owned(),
    }
}

pub fn trigger(selection: Selection, open: bool, cx: &App) -> (Stateful<Div>, String) {
    let Selection {
        provider,
        account,
        accounts,
        model,
        reported_model,
        last_model_name,
        default_model,
        started,
        choices,
        ..
    } = selection;
    let colors = cx.theme().colors;
    let (machine, machine_label) = selection.machine.unzip();
    let name = match account_label(provider, account, accounts, machine) {
        Some(label) => format!(
            "{} · {}",
            provider_name(provider),
            crate::settings_accounts::account_title(&label, machine_label)
        ),
        None => provider_name(provider).to_owned(),
    };
    let requested = requested_model(model, default_model, choices);
    // A requested model is shown as running only once the provider says so, in this
    // run or, saved, before a relaunch: until then the label is muted.
    let (label, color) = match (started, reported_model, last_model_name) {
        (true, Some(reported), _) => (choices.label(reported), Color::Default),
        (true, None, Some(last)) => (last.to_owned(), Color::Default),
        (true, None, None) => (requested, Color::Muted),
        (false, ..) => (requested, Color::Default),
    };
    let options = selection.options();
    // Effort and speed are what the session asks for, never what the provider
    // reported, so once it has sent they stay muted like an unreported model.
    let options_color = if started { Color::Muted } else { color };
    // The chip names the model; the provider is its icon, and the tooltip says both.
    let tooltip = match (started, options.is_empty()) {
        (true, true) => {
            format!("{name} · {label} · provider and account fixed after the first message")
        }
        (true, false) => format!(
            "{name} · {label}{options} · effort and speed as asked for the next turns · provider and account fixed after the first message"
        ),
        (false, _) => format!("{name} · {label}{options}"),
    };
    let trigger = h_flex()
        .id("model-selector")
        .automation_id("model-selector")
        .min_w(MIN_WIDTH)
        .h(cx.theme().density.control(px(28.)))
        .px(px(7.))
        .gap(px(5.))
        .rounded(radius::CONTROL)
        .child(
            Icon::new(provider_icon(provider))
                .size(px(14.))
                .color(provider_icon_color(provider, cx)),
        )
        .child(
            // The model name truncates first; the options are clipped only after it.
            h_flex()
                .min_w_0()
                .overflow_hidden()
                .child(Label::new(label).size(TextSize::Ui).color(color).truncate())
                .when(!options.is_empty(), |d| {
                    d.child(
                        div()
                            .flex_none()
                            .child(Label::new(options).size(TextSize::Ui).color(options_color)),
                    )
                }),
        );
    let hover = colors.element_filled_hover;
    let trigger = trigger
        .cursor_pointer()
        .bg(if open {
            colors.selection_fill
        } else {
            colors.element_filled
        })
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
    machine: Option<(&MachineId, &str)>,
    on: &Handlers,
    viewport: Size<Pixels>,
    cx: &App,
) -> impl IntoElement {
    let (machine, machine_label) = machine.unzip();
    // Room for "Default on desk", since each login there names its machine.
    let width = if machine.is_some() {
        px(204.)
    } else {
        px(152.)
    };
    v_flex().flex_none().w(width).child(
        v_flex()
            .id("provider-list")
            .debug_selector(|| "provider-list".into())
            .max_h(px(360.).min(viewport.height / 3.))
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
                let rows = accounts_of(provider, accounts, machine)
                    .into_iter()
                    .enumerate()
                    .map(|(at, (id, label))| {
                        let label = crate::settings_accounts::account_title(label, machine_label);
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
                                label,
                                provider == current && id == account,
                                false,
                                cx,
                            )
                            .on_click(move |_, window, cx| {
                                cx.stop_propagation();
                                choose(&chosen, window, cx)
                            }),
                        )
                    });
                v_flex().child(row).children(rows)
            })),
    )
}

fn model_column(
    selection: Selection,
    on: &Handlers,
    viewport: Size<Pixels>,
    cx: &App,
) -> impl IntoElement {
    let Selection {
        provider,
        account,
        model: chosen,
        choices,
        ..
    } = selection;
    let row = |id: ElementId, selector: String, label: SharedString, model: Option<String>| {
        let choose = on.choose_model.clone();
        let selected = chosen == model.as_deref();
        choice_row(id, selector, None, label, selected, false, cx).on_click(move |_, window, cx| {
            cx.stop_propagation();
            choose(&(provider, model.clone()), window, cx)
        })
    };
    let row_height = cx.theme().density.control(px(36.));
    let height = (row_height * CHOICE_ROWS).min(viewport.height / 3.);
    let column = v_flex()
        .id("model-column")
        .automation_id("model-column")
        .w(px(282.))
        .min_w_0()
        .pl(px(8.))
        // Once a sent session has a model, the provider keeps an override, so its own
        // default cannot be offered again.
        .when(offers_default(selection, chosen), |d| {
            d.child(row(
                "model-default".into(),
                "model-default".into(),
                "Provider default".into(),
                None,
            ))
        });
    let list = v_flex()
        .id("model-list")
        .automation_id("model-list")
        .flex_1()
        .min_h_0()
        .overflow_y_scroll();
    let (list, retry): (AnyElement, _) = match choices {
        Choices::Unavailable(reason) => (
            list.child(note("model-unavailable", reason, Color::Muted))
                .into_any_element(),
            None,
        ),
        Choices::Loading => (
            list.child(note("model-loading", "Loading choices…", Color::Muted))
                .into_any_element(),
            None,
        ),
        Choices::Ready(models) => (
            ModelList {
                catalog: format!(
                    "model-list-{}-{}",
                    slug(provider),
                    account.map_or_else(String::new, ToString::to_string)
                )
                .into(),
                reveal: models
                    .iter()
                    .position(|model| Some(model.model.as_str()) == chosen)
                    .map_or(px(0.), |at| {
                        (row_height * (at + 1) as f32 - height).max(px(0.))
                    }),
                list: list.children(models.iter().enumerate().map(|(index, model)| {
                    row(
                        ("model", index).into(),
                        format!("model-{}", model.model),
                        model.label.clone().into(),
                        Some(model.model.clone()),
                    )
                })),
            }
            .into_any_element(),
            None,
        ),
        Choices::Failed(message) => {
            let retry = on.retry.clone();
            (
                list.child(note("model-failed", message.to_owned(), Color::Danger))
                    .into_any_element(),
                Some(
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
                ),
            )
        }
    };
    column.child(v_flex().flex_none().h(height).child(list).children(retry))
}

/// Whether a field's Default row is offered: before the first send, or while the field
/// is still at Default.
fn offers_default(selection: Selection, value: Option<&str>) -> bool {
    !selection.started || value.is_none()
}

/// The effort and speed the session's model offers, each with its Default row while
/// that can be chosen.
fn options_column(
    selection: Selection,
    on: &OptionListeners,
    viewport: Size<Pixels>,
    cx: &App,
) -> impl IntoElement {
    let row_height = cx.theme().density.control(px(36.));
    // A narrow window takes room from this column before the models'.
    let column = v_flex()
        .id("choice-column")
        .automation_id("choice-column")
        .w(px(176.))
        .min_w(px(96.))
        .flex_shrink(4.)
        .pl(px(8.))
        // Every level and speed fits without scrolling in a usual window: the menu
        // opens upward from the composer, so it can take more height than the models.
        .max_h((row_height * OPTION_ROWS).min(viewport.height * 0.6))
        .overflow_y_scroll();
    if !matches!(selection.choices, Choices::Ready(_)) {
        return column;
    }
    let Some(entry) = selection.entry() else {
        return column.child(note("choice-unknown", NO_ENTRY, Color::Muted));
    };
    let section = |kind: &'static str,
                   title: &'static str,
                   current: Option<&str>,
                   values: Vec<(String, String)>,
                   choose: &Listener<Option<String>>| {
        let rows = offers_default(selection, current)
            .then(|| (None, "Default".to_owned()))
            .into_iter()
            .chain(
                values
                    .into_iter()
                    .map(|(value, label)| (Some(value), label)),
            )
            .enumerate()
            .map(|(at, (value, label))| {
                let choose = choose.clone();
                let selector = match &value {
                    Some(value) => format!("{kind}-{value}"),
                    None => format!("{kind}-default"),
                };
                let selected = current == value.as_deref();
                choice_row((kind, at), selector, None, label, selected, false, cx).on_click(
                    move |_, window, cx| {
                        cx.stop_propagation();
                        choose(&value, window, cx)
                    },
                )
            });
        v_flex()
            .child(
                div()
                    .px(px(8.))
                    .pt(px(6.))
                    .pb(px(2.))
                    .child(Label::new(title).size(TextSize::Ui).color(Color::Muted)),
            )
            .children(rows)
    };
    let efforts = entry
        .efforts
        .iter()
        .map(|effort| (effort.clone(), effort.clone()))
        .collect::<Vec<_>>();
    let speeds = entry
        .speeds
        .iter()
        .map(|speed| (speed.value.clone(), speed.label.clone()))
        .collect::<Vec<_>>();
    if efforts.is_empty() && speeds.is_empty() {
        return column.child(note(
            "choice-none",
            "This model has no effort or speed choices",
            Color::Muted,
        ));
    }
    column
        .when(!efforts.is_empty(), |d| {
            d.child(section(
                "effort",
                "Effort",
                selection.effort,
                efforts,
                &on.effort,
            ))
        })
        .when(!speeds.is_empty(), |d| {
            d.child(section(
                "speed",
                "Speed",
                selection.speed,
                speeds,
                &on.speed,
            ))
        })
}

/// A provider's model choices, scrolled when they first show just far enough to show the
/// session's model, as `Select` does. Later renders keep wherever the user scrolled.
#[derive(IntoElement)]
struct ModelList {
    /// The provider account whose catalog this is: another one starts its own reveal.
    catalog: SharedString,
    list: Stateful<Div>,
    /// How far down the session's model sits below the fold.
    reveal: Pixels,
}

impl RenderOnce for ModelList {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let reveal = self.reveal;
        let scroll = window
            .use_keyed_state(self.catalog, cx, |_, _| {
                let scroll = ScrollHandle::new();
                scroll.set_offset(point(px(0.), -reveal));
                scroll
            })
            .read(cx)
            .clone();
        self.list.track_scroll(&scroll)
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use keystone_agent::Speed;

    fn model(name: &str, efforts: &[&str], default_effort: Option<&str>, fast: bool) -> ModelInfo {
        ModelInfo {
            model: name.into(),
            label: name.into(),
            efforts: efforts.iter().map(|e| (*e).into()).collect(),
            default_effort: default_effort.map(Into::into),
            speeds: if fast {
                ["standard", "fast"]
                    .map(|value| Speed {
                        value: value.into(),
                        label: value.into(),
                    })
                    .into()
            } else {
                Vec::new()
            },
            ..ModelInfo::default()
        }
    }

    fn switched(
        provider: Provider,
        models: &[ModelInfo],
        from: &str,
        effort: &str,
        speed: &str,
        to: &str,
        started: bool,
    ) -> ModelChoice {
        let selection = Selection {
            provider,
            account: None,
            accounts: &[],
            machine: None,
            model: Some(from),
            effort: Some(effort),
            speed: Some(speed),
            reported_model: None,
            last_model_name: None,
            default_model: None,
            started,
            choices: Choices::Ready(models),
        };
        let next = models.iter().find(|m| m.model == to);
        switch_model(&selection, Some(to.into()), next)
    }

    #[test]
    fn after_the_first_send_a_chosen_effort_or_speed_never_returns_to_default() {
        let claude = Provider::Claude;
        let all = ["low", "medium", "high", "xhigh", "max"];
        let models = [
            model("opus", &all, None, true),
            model("older", &["low", "medium", "high", "max"], None, false),
            model("haiku-4-5", &[], None, false),
            model("codexish", &["low", "high"], Some("high"), true),
        ];
        let choice = |effort: &str, speed: &str| ModelChoice {
            model: None,
            effort: Some(effort.into()),
            speed: Some(speed.into()),
        };
        let without_model = |c: ModelChoice| ModelChoice { model: None, ..c };

        // A level the new model lacks becomes the nearest lower one it lists, and an
        // unlisted Fast becomes standard speed, which the provider is told.
        let after = switched(claude, &models, "opus", "xhigh", "fast", "older", true);
        assert_eq!(without_model(after), choice("high", "standard"));
        // A model without effort levels keeps the effort, which it does not use, and
        // the next model that lists levels ranks it as the catalog does.
        let after = switched(claude, &models, "opus", "xhigh", "fast", "haiku-4-5", true);
        assert_eq!(without_model(after), choice("xhigh", "standard"));
        let after = switched(
            claude,
            &models,
            "haiku-4-5",
            "xhigh",
            "standard",
            "older",
            true,
        );
        assert_eq!(without_model(after), choice("high", "standard"));
        // A listed default effort wins over the nearest level.
        let after = switched(claude, &models, "opus", "xhigh", "fast", "codexish", true);
        assert_eq!(without_model(after), choice("high", "fast"));
        // Codex's standard tier is its own `default`.
        let after = switched(
            Provider::Codex,
            &models,
            "codexish",
            "high",
            "fast",
            "older",
            true,
        );
        assert_eq!(without_model(after), choice("high", "default"));
        // A model the catalog does not know keeps everything.
        let after = switched(claude, &models, "opus", "xhigh", "fast", "unknown", true);
        assert_eq!(without_model(after), choice("xhigh", "fast"));

        // Before the first send nothing was sent, so Default is still the honest answer.
        let after = switched(claude, &models, "opus", "xhigh", "fast", "older", false);
        assert_eq!((after.effort, after.speed), (None, None));
    }

    #[test]
    fn the_button_and_the_note_show_only_what_the_model_uses() {
        let models = [
            model("opus", &["low", "high"], None, true),
            model("haiku-4-5", &[], None, false),
        ];
        let shown = |model: &str, effort: &str, speed: &str| {
            let selection = Selection {
                provider: Provider::Claude,
                account: None,
                accounts: &[],
                machine: None,
                model: Some(model),
                effort: Some(effort),
                speed: Some(speed),
                reported_model: None,
                last_model_name: None,
                default_model: None,
                started: true,
                choices: Choices::Ready(&models),
            };
            (selection.options(), selection.note())
        };
        assert_eq!(
            shown("opus", "high", "fast"),
            (
                " · high · fast".into(),
                "Next turns: opus · effort high · fast".into()
            )
        );
        // Standard speed is left out of the button, not the note.
        assert_eq!(
            shown("opus", "high", "standard"),
            (
                " · high".into(),
                "Next turns: opus · effort high · standard".into()
            )
        );
        // A model without levels or speeds shows neither the kept effort nor the
        // standard speed it is sent.
        assert_eq!(
            shown("haiku-4-5", "high", "standard"),
            (String::new(), "Next turns: haiku-4-5".into())
        );
        // A model the catalog does not know shows what was chosen.
        assert_eq!(
            shown("unknown", "high", "fast"),
            (
                " · high · fast".into(),
                "Next turns: unknown · effort high · fast".into()
            )
        );
    }
}
