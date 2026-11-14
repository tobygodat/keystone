//! Settings → Accounts: each provider's accounts, the command that signs each one in,
//! and a short form to add one. Keystone never signs anyone in or reads credentials;
//! it only remembers which folder each account's CLI uses.

use crate::Keystone;
use crate::model_selector::provider_name;
use crate::settings_page::SettingsPage;
use gpui::{
    AnyElement, App, AppContext, ClipboardItem, Context, Entity, FocusHandle, Focusable,
    FontWeight, KeyDownEvent, Window, div, prelude::*, px,
};
use keystone_agent::{
    default_home, default_sign_in_command, remote_sign_in_command, sign_in_command,
};
use keystone_core::{Account, AccountId, MachineId, Provider};
use keystone_ui::prelude::*;
use keystone_ui::text_input::{InputMode, SelectAll, TextInput};
use std::cell::RefCell;
use std::collections::HashMap;

const PROVIDERS: [Provider; 2] = [Provider::Claude, Provider::Codex];

/// The section's own state, kept while the settings page is open.
pub(crate) struct AccountsPage {
    provider: Provider,
    provider_focus: FocusHandle,
    /// The machine a new account is a login on, `None` for this one.
    machine: Option<MachineId>,
    machine_focus: FocusHandle,
    name: Entity<TextInput>,
    folder: Entity<TextInput>,
    add: FocusHandle,
    /// The account being renamed in place, and its field.
    renaming: Option<AccountId>,
    rename: Entity<TextInput>,
    /// The row whose sign-in command was just copied, shown with a check.
    copied: Option<String>,
    /// Focus for each row's buttons, made as rows first render.
    stops: RefCell<HashMap<String, FocusHandle>>,
}

fn field(placeholder: &str, cx: &mut App) -> Entity<TextInput> {
    let input = cx.new(|cx| TextInput::new(placeholder, InputMode::SingleLine, cx));
    let _ = input.read(cx).focus_handle(cx).tab_stop(true);
    input
}

impl AccountsPage {
    pub(crate) fn new(cx: &mut App) -> Self {
        Self {
            provider: Provider::Claude,
            provider_focus: cx.focus_handle().tab_stop(true),
            machine: None,
            machine_focus: cx.focus_handle().tab_stop(true),
            name: field("Name", cx),
            folder: field("Folder", cx),
            add: cx.focus_handle().tab_stop(true),
            renaming: None,
            rename: field("Account name", cx),
            copied: None,
            stops: RefCell::default(),
        }
    }

    fn stop(&self, id: &str, cx: &App) -> FocusHandle {
        self.stops
            .borrow_mut()
            .entry(id.to_owned())
            .or_insert_with(|| cx.focus_handle().tab_stop(true))
            .clone()
    }
}

fn provider_slug(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
    }
}

/// One row: the default account (`account` is `None`) or an added one, with the
/// label of the machine it is a login on.
struct Row<'a> {
    provider: Provider,
    account: Option<&'a Account>,
    machine: Option<String>,
    sessions: usize,
}

/// How a row names an account: its label, and for a login on another machine, which
/// machine, so each row reads as one login in one place.
pub(crate) fn account_title(label: &str, machine: Option<&str>) -> String {
    match machine {
        Some(machine) => format!("{label} on {machine}"),
        None => label.to_owned(),
    }
}

impl Keystone {
    fn accounts_page(&mut self) -> Option<&mut AccountsPage> {
        self.settings.as_mut().map(|page| &mut page.accounts)
    }

    /// The Accounts section's content, below its title.
    pub(crate) fn accounts_section(
        &self,
        page: &SettingsPage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let workspace = self.state.read(cx).store().workspace();
        let added: Vec<(Account, usize, Option<String>)> = workspace
            .accounts()
            .iter()
            .map(|account| {
                let sessions = workspace
                    .sessions()
                    .iter()
                    .filter(|s| s.account() == Some(account.id()))
                    .count();
                let machine = account.machine().map(|id| {
                    workspace
                        .machine(id)
                        .map_or_else(|| id.to_string(), |m| m.label().to_owned())
                });
                (account.clone(), sessions, machine)
            })
            .collect();
        let groups: Vec<_> = PROVIDERS
            .into_iter()
            .map(|provider| {
                let rows = std::iter::once(Row {
                    provider,
                    account: None,
                    machine: None,
                    sessions: 0,
                })
                .chain(added.iter().filter(|(a, ..)| a.provider() == provider).map(
                    |(account, sessions, machine)| Row {
                        provider,
                        account: Some(account),
                        machine: machine.clone(),
                        sessions: *sessions,
                    },
                ))
                .map(|row| self.account_row(&page.accounts, row, cx))
                .collect::<Vec<_>>();
                v_flex()
                    .pt(spacing::LG)
                    .child(
                        Label::new(provider_name(provider))
                            .size(TextSize::Ui)
                            .weight(FontWeight::MEDIUM)
                            .color(Color::Muted),
                    )
                    .children(rows)
            })
            .collect();
        v_flex()
            .automation_id("settings-accounts")
            .children(groups)
            .child(self.add_account_form(&page.accounts, cx))
            .into_any_element()
    }

    fn account_row(
        &self,
        page: &AccountsPage,
        row: Row,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let Row {
            provider,
            account,
            machine,
            sessions,
        } = row;
        let key = account.map_or_else(
            || format!("{}-default", provider_slug(provider)),
            |account| account.id().to_string(),
        );
        let command = match account {
            None => default_sign_in_command(provider),
            Some(account) => match (account.home(), account.remote_home()) {
                (Some(home), _) => sign_in_command(provider, Some(home)),
                (None, Some((_, home))) => remote_sign_in_command(provider, &home),
                (None, None) => String::new(),
            },
        };
        let detail = match (account, default_home(provider)) {
            (None, Some(home)) => home.display().to_string(),
            _ => command.clone(),
        };
        let copied = page.copied.as_deref() == Some(key.as_str());
        let copy = {
            let (command, key) = (command.clone(), key.clone());
            IconButton::new(
                format!("account-copy-{key}"),
                if copied {
                    IconName::Check
                } else {
                    IconName::Copy
                },
            )
            .style(ButtonStyle::Subtle)
            .size(ButtonSize::Compact)
            .track_focus(&page.stop(&format!("copy-{key}"), cx))
            .debug_selector(|| format!("account-copy-{key}"))
            .aria_label("Copy sign-in command")
            .tooltip(Tooltip::text(match (copied, &machine) {
                (true, _) => "Copied".to_owned(),
                (false, Some(machine)) => format!("Copy the sign-in command to run on {machine}"),
                (false, None) => "Copy sign-in command".to_owned(),
            }))
            .on_click(self.scoped_listener(cx, move |this, _, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(command.clone()));
                if let Some(page) = this.accounts_page() {
                    page.copied = Some(key.clone());
                }
                cx.notify();
            }))
        };
        let renaming = account.is_some_and(|a| page.renaming.as_ref() == Some(a.id()));
        let title: AnyElement = if renaming {
            let submit = self.scoped_listener(cx, |this, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "enter" => this.confirm_account_rename(window, cx),
                    "escape" => this.cancel_account_rename(window, cx),
                    _ => return,
                }
                cx.stop_propagation();
            });
            div()
                .px(px(8.))
                .rounded(px(6.))
                .border_1()
                .border_color(cx.theme().colors.border_focused)
                .bg(cx.theme().colors.background)
                .automation_id("account-rename-field")
                .on_key_down(submit)
                .child(page.rename.clone())
                .into_any_element()
        } else {
            h_flex()
                .gap_2()
                .child(
                    Label::new(account_title(
                        account.map_or("Default", Account::label),
                        machine.as_deref(),
                    ))
                    .truncate(),
                )
                .when(sessions > 0, |d| {
                    d.child(
                        Label::new(format!(
                            "{sessions} session{}",
                            if sessions == 1 { "" } else { "s" }
                        ))
                        .size(TextSize::Sm)
                        .color(Color::Faint),
                    )
                })
                .into_any_element()
        };
        let mut actions = h_flex().flex_none().gap_1().child(copy);
        if let Some(account) = account {
            let (rename_id, remove_id) = (account.id().clone(), account.id().clone());
            actions = actions
                .child(
                    IconButton::new(format!("account-rename-{key}"), IconName::Pencil)
                        .style(ButtonStyle::Subtle)
                        .size(ButtonSize::Compact)
                        .track_focus(&page.stop(&format!("rename-{key}"), cx))
                        .debug_selector(|| format!("account-rename-{key}"))
                        .aria_label("Rename account")
                        .tooltip(Tooltip::text("Rename"))
                        .on_click(self.scoped_listener(cx, move |this, _, window, cx| {
                            this.start_account_rename(&rename_id, window, cx)
                        })),
                )
                .child(
                    IconButton::new(format!("account-remove-{key}"), IconName::Close)
                        .style(ButtonStyle::Subtle)
                        .size(ButtonSize::Compact)
                        .disabled(sessions > 0)
                        .track_focus(&page.stop(&format!("remove-{key}"), cx))
                        .debug_selector(|| format!("account-remove-{key}"))
                        .aria_label("Remove account")
                        .tooltip(Tooltip::text(if sessions > 0 {
                            "In use"
                        } else {
                            "Remove (folder stays on disk)"
                        }))
                        .on_click(self.scoped_listener(cx, move |this, _, _, cx| {
                            this.remove_account(&remove_id, cx)
                        })),
                );
        }
        h_flex()
            .automation_id(format!("account-row-{key}"))
            .gap(spacing::MD)
            .py(spacing::SM)
            .hairline_t(cx)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(title)
                    .child(
                        Label::new(detail)
                            .size(TextSize::Sm)
                            .color(Color::Muted)
                            .mono()
                            .truncate(),
                    )
                    .child(self.usage_line(&key, provider, account.map(Account::id), cx)),
            )
            .child(actions)
    }

    fn add_account_form(
        &self,
        page: &AccountsPage,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let colors = cx.theme().colors;
        let input = |entity: &Entity<TextInput>, id: &'static str| {
            div()
                .flex_1()
                .min_w(px(120.))
                .px(px(8.))
                .rounded(px(6.))
                .border_1()
                .border_color(colors.border)
                .bg(colors.background)
                .automation_id(id)
                .child(entity.clone())
        };
        let submit = self.scoped_listener(cx, |this, event: &KeyDownEvent, window, cx| {
            if event.keystroke.key == "enter" {
                this.add_account(window, cx);
                cx.stop_propagation();
            }
        });
        // Which machine the login is on, offered once another machine is linked.
        let linked = self.state.read(cx).store().workspace().machines();
        let machines = (!linked.is_empty()).then(|| {
            let at = page
                .machine
                .as_ref()
                .and_then(|id| linked.iter().position(|m| m.id() == id))
                .map_or(0, |at| at + 1);
            let labels: Vec<String> = std::iter::once("This machine".to_owned())
                .chain(linked.iter().map(|m| m.label().to_owned()))
                .collect();
            (labels, at)
        });
        v_flex()
            .pt(spacing::LG)
            .gap(spacing::SM)
            .child(
                Label::new("Add an account")
                    .size(TextSize::Ui)
                    .weight(FontWeight::MEDIUM)
                    .color(Color::Muted),
            )
            .child(
                v_flex()
                    .gap(spacing::SM)
                    .on_key_down(submit)
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap(spacing::SM)
                            .child(
                                div().flex_none().child(
                                    Select::new(
                                        "account-provider",
                                        PROVIDERS.map(provider_name),
                                        PROVIDERS.iter().position(|p| *p == page.provider),
                                    )
                                    .width(px(136.))
                                    .track_focus(&page.provider_focus)
                                    .on_change(
                                        self.scoped_listener(cx, |this, &at: &usize, _, cx| {
                                            if let Some(page) = this.accounts_page() {
                                                page.provider = PROVIDERS[at];
                                            }
                                            cx.notify();
                                        }),
                                    ),
                                ),
                            )
                            .when_some(machines, |d, (labels, at)| {
                                d.child(
                                    div().flex_none().child(
                                        Select::new("account-machine", labels, Some(at))
                                            .width(px(160.))
                                            .track_focus(&page.machine_focus)
                                            .on_change(self.scoped_listener(
                                                cx,
                                                move |this, &at: &usize, _, cx| {
                                                    let machine =
                                                        at.checked_sub(1).and_then(|at| {
                                                            this.state
                                                                .read(cx)
                                                                .store()
                                                                .workspace()
                                                                .machines()
                                                                .get(at)
                                                                .map(|m| m.id().clone())
                                                        });
                                                    if let Some(page) = this.accounts_page() {
                                                        page.machine = machine;
                                                    }
                                                    cx.notify();
                                                },
                                            )),
                                    ),
                                )
                            })
                            .child(input(&page.name, "account-name")),
                    )
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap(spacing::SM)
                            .child(input(&page.folder, "account-folder"))
                            .child(
                                div().flex_none().child(
                                    Button::new("account-add", "Add")
                                        .style(ButtonStyle::Filled)
                                        .track_focus(&page.add)
                                        .debug_selector(|| "account-add".into())
                                        .on_click(
                                            self.scoped_listener(cx, |this, _, window, cx| {
                                                this.add_account(window, cx)
                                            }),
                                        ),
                                ),
                            ),
                    ),
            )
    }

    fn add_account(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(page) = &self.settings else { return };
        let page = &page.accounts;
        let provider = page.provider;
        let machine = page.machine.clone();
        let name = page.name.read(cx).value().to_owned();
        let folder = page.folder.read(cx).value().to_owned();
        let (name_input, folder_input) = (page.name.clone(), page.folder.clone());
        match self.state.update(cx, |state, cx| match &machine {
            Some(machine) => state.add_remote_account(provider, &name, &folder, machine, cx),
            None => state.add_account(provider, &name, &folder, cx),
        }) {
            Ok(_) => {
                self.error = None;
                for input in [&name_input, &folder_input] {
                    input.update(cx, |input, cx| input.set_value(String::new(), cx));
                }
                name_input.focus_handle(cx).focus(window, cx);
            }
            Err(error) => self.error = Some(format!("Account not added. {error}")),
        }
        self.refresh(cx);
    }

    fn start_account_rename(
        &mut self,
        id: &AccountId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(label) = self
            .state
            .read(cx)
            .store()
            .workspace()
            .account(id)
            .map(|a| a.label().to_owned())
        else {
            return;
        };
        let Some(page) = self.accounts_page() else {
            return;
        };
        page.renaming = Some(id.clone());
        let rename = page.rename.clone();
        rename.update(cx, |input, cx| {
            input.set_value(label, cx);
            input.select_all(&SelectAll, window, cx);
        });
        rename.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn cancel_account_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(page) = self.accounts_page() else {
            return;
        };
        let Some(id) = page.renaming.take() else {
            return;
        };
        let stop = page.stop(&format!("rename-{id}"), cx);
        window.focus(&stop, cx);
        cx.notify();
    }

    fn confirm_account_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(page) = &self.settings else { return };
        let Some(id) = page.accounts.renaming.clone() else {
            return;
        };
        let label = page.accounts.rename.read(cx).value().to_owned();
        match self
            .state
            .update(cx, |state, cx| state.rename_account(&id, &label, cx))
        {
            Ok(_) => {
                self.error = None;
                self.cancel_account_rename(window, cx);
            }
            Err(error) => self.error = Some(format!("Account not renamed. {error}")),
        }
        self.refresh(cx);
    }

    fn remove_account(&mut self, id: &AccountId, cx: &mut Context<Self>) {
        self.error = match self
            .state
            .update(cx, |state, cx| state.remove_account(id, cx))
        {
            Ok(_) => None,
            Err(error) => Some(format!("Account not removed. {error}")),
        };
        self.refresh(cx);
    }
}
