//! Settings → Integrations: the MCP servers, skills and plugins each provider account
//! loads for the window's project, read from the providers' own files, and what
//! running sessions last reported about each server. Nothing here writes.

use crate::Keystone;
use crate::app_state::ReportedStatus;
use crate::model_selector::provider_name;
use crate::settings_page::SettingsPage;
use gpui::{AnyElement, App, Context, FocusHandle, FontWeight, Hsla, div, prelude::*, px};
use keystone_core::integrations::{self, Detail, Health, Inventory, Item, Kind, State, Transport};
use keystone_core::{AccountId, Provider};
use keystone_ui::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Numbers each read, so only the newest one fills the page that asked for it.
static READS: AtomicU64 = AtomicU64::new(0);

const KINDS: [(Kind, &str, &str); 3] = [
    (Kind::McpServer, "MCP servers", "mcp"),
    (Kind::Skill, "Skills", "skill"),
    (Kind::Plugin, "Plugins", "plugin"),
];

/// One account's inventory, or `None` when its folder is unknown.
pub(crate) struct AccountInventory {
    provider: Provider,
    account: Option<AccountId>,
    label: String,
    inventory: Option<Inventory>,
}

pub(crate) struct IntegrationsPage {
    /// What the last read found, in the order Settings → Accounts lists accounts.
    read: Option<Vec<AccountInventory>>,
    reading: bool,
    /// The read this page waits for; an older one that finishes later is dropped.
    ticket: u64,
    refresh: FocusHandle,
}

impl IntegrationsPage {
    pub(crate) fn new(cx: &mut App) -> Self {
        Self {
            read: None,
            reading: false,
            ticket: 0,
            refresh: cx.focus_handle().tab_stop(true),
        }
    }
}

fn provider_slug(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
    }
}

fn row_id(
    provider: Provider,
    account: Option<&AccountId>,
    kind: &str,
    scope: &str,
    name: &str,
) -> String {
    format!(
        "integration-{}-{}-{kind}-{scope}-{name}",
        provider_slug(provider),
        account.map_or(AccountId::DEFAULT, AccountId::as_str),
    )
}

/// `path` with the user's home shown as `~`.
fn shown_path(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Where an item comes from, name first, so a narrow window cuts the folder rather
/// than the file: `settings.json in ~/.claude`.
fn source_label(path: &Path, home: Option<&Path>) -> String {
    match (path.file_name(), path.parent()) {
        (Some(name), Some(folder)) => {
            format!("{} in {}", name.to_string_lossy(), shown_path(folder, home))
        }
        _ => shown_path(path, home),
    }
}

/// What the row says beside an item's name when the provider does not use it.
fn note(item: &Item) -> Option<String> {
    if let Some(scope) = item.overridden_by {
        return Some(format!("Overridden by {} scope", scope.label()));
    }
    match item.state {
        State::On => None,
        State::Off => Some("Off".into()),
        State::Untrusted => Some("Project not trusted by Codex".into()),
    }
}

/// The line under an item's name: how a server runs and the names of what it is
/// given, shown as code, a skill's description, or a plugin's version.
fn detail(item: &Item) -> String {
    match &item.detail {
        Detail::McpServer(server) => {
            let mut text = match &server.transport {
                Transport::Stdio { command, args: 0 } => command.clone(),
                Transport::Stdio { command, args: 1 } => format!("{command} · 1 argument"),
                Transport::Stdio { command, args } => format!("{command} · {args} arguments"),
                Transport::Remote { kind, url } => format!("{url} ({kind})"),
            };
            if !server.env.is_empty() {
                text.push_str(&format!(" · env {}", server.env.join(", ")));
            }
            if !server.headers.is_empty() {
                text.push_str(&format!(" · headers {}", server.headers.join(", ")));
            }
            text
        }
        Detail::Skill { description, .. } => description.clone(),
        Detail::Plugin {
            version: Some(version),
        } => format!("Version {version}"),
        Detail::Plugin { version: None } => String::new(),
    }
}

fn health_color(health: Health) -> Color {
    match health {
        Health::Connected => Color::Success,
        Health::Pending | Health::NeedsAuth => Color::Warning,
        Health::Failed => Color::Danger,
        Health::Disabled => Color::Faint,
    }
}

fn dot(color: Hsla) -> impl IntoElement {
    div().size(px(6.)).rounded_full().flex_none().bg(color)
}

impl Keystone {
    /// Reads every account's files for the window's project off the UI thread, then
    /// shows them.
    pub(crate) fn read_integrations(&mut self, cx: &mut Context<Self>) {
        let Some(page) = self.settings.as_mut() else {
            return;
        };
        let ticket = READS.fetch_add(1, Ordering::Relaxed) + 1;
        page.integrations.ticket = ticket;
        page.integrations.reading = true;
        let state = self.state.read(cx);
        let env = state.config_env.clone();
        let mut accounts: Vec<(Provider, Option<AccountId>, String, Option<PathBuf>)> = Vec::new();
        for provider in [Provider::Claude, Provider::Codex] {
            accounts.push((provider, None, "Default".into(), None));
            accounts.extend(
                state
                    .store()
                    .workspace()
                    .accounts()
                    .iter()
                    .filter(|a| a.provider() == provider)
                    .map(|a| {
                        (
                            provider,
                            Some(a.id().clone()),
                            a.label().to_owned(),
                            Some(a.home().to_owned()),
                        )
                    }),
            );
        }
        let project = self
            .presentation
            .project
            .as_ref()
            .ok()
            .map(|p| p.path().to_owned());
        let read = cx.background_executor().spawn(async move {
            accounts
                .into_iter()
                .map(|(provider, account, label, home)| {
                    let inventory = project
                        .as_deref()
                        .zip(env.location(provider, home.as_deref()))
                        .map(|(project, location)| {
                            let id = account
                                .clone()
                                .unwrap_or_else(|| AccountId::new(AccountId::DEFAULT));
                            integrations::read(&id, &location, project)
                        });
                    AccountInventory {
                        provider,
                        account,
                        label,
                        inventory,
                    }
                })
                .collect::<Vec<_>>()
        });
        cx.spawn(async move |this, cx| {
            let read = read.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(page) = this.settings.as_mut()
                    && page.integrations.ticket == ticket
                {
                    page.integrations.read = Some(read);
                    page.integrations.reading = false;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// The Integrations section's content, below its title.
    pub(crate) fn integrations_section(
        &self,
        page: &SettingsPage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let integrations = &page.integrations;
        let state = self.state.read(cx);
        let home = state.config_env.home.clone();
        let project = self.presentation.project.as_ref().ok().map(|p| p.path());
        let now = crate::now_ms();
        let refresh = Button::new(
            "integrations-refresh",
            if integrations.reading {
                "Reading…"
            } else {
                "Read again"
            },
        )
        .style(ButtonStyle::Subtle)
        .size(ButtonSize::Compact)
        .disabled(integrations.reading)
        .track_focus(&integrations.refresh)
        .debug_selector(|| "integrations-refresh".into())
        .on_click(self.scoped_listener(cx, |this, _, _, cx| this.read_integrations(cx)));
        let intro = v_flex()
            .gap(spacing::SM)
            .child(
                Label::new(
                    "What each account's provider loads for this project, read from its own files. Keystone only reads them here. Secret values are never shown: a server's arguments are only counted, its variables and headers only named, and its URL cut to the host. A server's status is what a running session in this project last reported.",
                )
                .size(TextSize::Ui)
                .color(Color::Muted),
            )
            .child(h_flex().child(refresh));
        let Some(read) = &integrations.read else {
            return v_flex()
                .automation_id("settings-integrations")
                .child(intro)
                .child(
                    div().pt(spacing::LG).child(
                        Label::new("Reading…")
                            .size(TextSize::Ui)
                            .color(Color::Faint),
                    ),
                )
                .into_any_element();
        };
        let mut rows: BTreeMap<Kind, Vec<AnyElement>> = BTreeMap::new();
        let mut problems = Vec::new();
        for account in read {
            let who = format!("{} · {}", provider_name(account.provider), account.label);
            let Some(inventory) = &account.inventory else {
                problems.push(format!("{who}: its folder is unknown, so nothing was read"));
                continue;
            };
            let reported = project
                .map(|project| {
                    state.mcp_status(account.provider, account.account.as_ref(), project)
                })
                .unwrap_or_default();
            let mut seen: HashMap<String, usize> = HashMap::new();
            for item in &inventory.items {
                let kind = KINDS.iter().find(|(k, ..)| *k == item.kind()).unwrap();
                let mut id = row_id(
                    item.provider,
                    account.account.as_ref(),
                    kind.2,
                    &item.scope.label().to_lowercase(),
                    &item.name,
                );
                let repeat = seen.entry(id.clone()).or_default();
                *repeat += 1;
                if *repeat > 1 {
                    id = format!("{id}-{repeat}");
                }
                let source = match &item.detail {
                    Detail::Skill {
                        plugin: Some(plugin),
                        ..
                    } => format!("from {plugin}"),
                    _ => source_label(&item.source, home.as_deref()),
                };
                let meta = format!("{who} · {} · {source}", item.scope.label());
                let used = item.overridden_by.is_none() && item.state != State::Untrusted;
                let status = (item.kind() == Kind::McpServer).then(|| {
                    Self::status(
                        &id,
                        reported.get(&item.name).filter(|_| used),
                        used && item.state == State::On,
                        now,
                        cx,
                    )
                });
                rows.entry(item.kind())
                    .or_default()
                    .push(Self::integration_row(
                        &id,
                        &item.name,
                        note(item),
                        (detail(item), item.kind() == Kind::McpServer),
                        meta,
                        status,
                        cx,
                    ));
            }
            for (name, status) in &reported {
                let declared = inventory
                    .items
                    .iter()
                    .any(|i| i.kind() == Kind::McpServer && &i.name == name);
                if declared {
                    continue;
                }
                let id = row_id(
                    account.provider,
                    account.account.as_ref(),
                    "mcp",
                    "reported",
                    name,
                );
                let status_element = Self::status(&id, Some(status), true, now, cx);
                rows.entry(Kind::McpServer)
                    .or_default()
                    .push(Self::integration_row(
                        &id,
                        name,
                        None,
                        (
                            "Not in a file Keystone reads, such as a plugin's or a connector"
                                .into(),
                            false,
                        ),
                        format!("{who} · Reported by a running session"),
                        Some(status_element),
                        cx,
                    ));
            }
            for problem in &inventory.problems {
                problems.push(format!(
                    "{who}: could not read {}: {}",
                    shown_path(&problem.path, home.as_deref()),
                    problem.message
                ));
            }
        }
        let groups = KINDS.iter().map(|&(kind, title, slug)| {
            let rows = rows.remove(&kind).unwrap_or_default();
            let count = rows.len();
            v_flex()
                .automation_id(format!("integrations-{slug}"))
                .pt(spacing::LG)
                .child(
                    h_flex()
                        .gap_2()
                        .pb(spacing::SM)
                        .child(
                            Label::new(title)
                                .size(TextSize::Ui)
                                .weight(FontWeight::MEDIUM)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(count.to_string())
                                .size(TextSize::Sm)
                                .color(Color::Faint),
                        ),
                )
                .when(count == 0, |d| {
                    d.child(
                        div().py(spacing::SM).hairline_t(cx).child(
                            Label::new("None found")
                                .size(TextSize::Sm)
                                .color(Color::Faint),
                        ),
                    )
                })
                .children(rows)
        });
        v_flex()
            .automation_id("settings-integrations")
            .child(intro)
            .children(groups)
            .when(!problems.is_empty(), |d| {
                d.child(
                    v_flex()
                        .automation_id("integrations-problems")
                        .pt(spacing::LG)
                        .gap(px(2.))
                        .children(problems.into_iter().map(|problem| {
                            Label::new(problem).size(TextSize::Sm).color(Color::Warning)
                        })),
                )
            })
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn integration_row(
        id: &str,
        name: &str,
        note: Option<String>,
        (detail, mono): (String, bool),
        meta: String,
        status: Option<AnyElement>,
        cx: &App,
    ) -> AnyElement {
        h_flex()
            .automation_id(id.to_owned())
            .gap(spacing::MD)
            .py(spacing::SM)
            .hairline_t(cx)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Label::new(name.to_owned()).truncate())
                            .children(note.map(|note| {
                                Label::new(note).size(TextSize::Sm).color(Color::Faint)
                            })),
                    )
                    .when(!detail.is_empty(), |d| {
                        let label = Label::new(detail).size(TextSize::Sm).color(Color::Muted);
                        d.child(if mono { label.mono() } else { label }.truncate())
                    })
                    .child(
                        Label::new(meta)
                            .size(TextSize::Sm)
                            .color(Color::Faint)
                            .truncate(),
                    ),
            )
            .children(status)
            .into_any_element()
    }

    /// A server's status on the right of its row: what a session in this project
    /// reported, with its age in the tooltip, or "Not running" when none has and the
    /// provider would use it.
    fn status(
        id: &str,
        reported: Option<&ReportedStatus>,
        used: bool,
        now: u64,
        cx: &App,
    ) -> AnyElement {
        let (label, color, tooltip) = match reported {
            Some(reported) => {
                let health = reported.status.health;
                let tooltip = format!("Reported {}", crate::usage::age(now, reported.at_ms));
                (health.label(), health_color(health), Some(tooltip))
            }
            None if used => ("Not running", Color::Faint, None),
            None => ("", Color::Faint, None),
        };
        let shown = reported.map_or(
            if label.is_empty() {
                "none"
            } else {
                "not-running"
            },
            |r| match r.status.health {
                Health::Connected => "connected",
                Health::Pending => "starting",
                Health::NeedsAuth => "needs-sign-in",
                Health::Failed => "failed",
                Health::Disabled => "off",
            },
        );
        let element = h_flex()
            .id(format!("{id}-status"))
            .automation_id(format!("{id}-status-{shown}"))
            .flex_none()
            .gap(px(6.))
            .when(!label.is_empty(), |d| d.child(dot(color.to_hsla(cx))))
            .child(
                Label::new(label)
                    .size(TextSize::Sm)
                    .color(if reported.is_some() {
                        color
                    } else {
                        Color::Faint
                    }),
            );
        match tooltip {
            Some(tooltip) => element
                .tooltip(Tooltip::text(tooltip).builder())
                .into_any_element(),
            None => element.into_any_element(),
        }
    }
}

#[cfg(all(test, feature = "ui-tests"))]
#[path = "integrations_tests.rs"]
mod ui_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_name_the_file_first_and_shorten_the_home() {
        let home = Some(Path::new("/home/u"));
        assert_eq!(
            source_label(Path::new("/home/u/.claude/settings.json"), home),
            "settings.json in ~/.claude"
        );
        assert_eq!(
            source_label(Path::new("/home/u/.claude.json"), home),
            ".claude.json in ~"
        );
        assert_eq!(
            source_label(Path::new("/srv/p/.mcp.json"), home),
            ".mcp.json in /srv/p"
        );
        assert_eq!(
            shown_path(Path::new("/home/user2/x"), home),
            "/home/user2/x"
        );
    }
}
