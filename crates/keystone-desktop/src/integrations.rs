//! Settings → Integrations: the MCP servers, skills and plugins each provider account
//! loads for the window's project, read from the providers' own files, and what
//! running sessions last reported about each server. Servers are added, edited,
//! switched and removed here, plugins switched, and skill folders added and removed,
//! through `keystone_core::integrations::edit`, and servers and skill folders copied
//! to another account through `keystone_core::integrations::copy`.

use crate::Keystone;
use crate::app_state::ReportedStatus;
use crate::model_selector::provider_name;
use crate::settings_page::SettingsPage;
use gpui::{
    AnyElement, App, AppContext, Context, Entity, FocusHandle, Focusable, FontWeight, Hsla,
    KeyDownEvent, Window, div, prelude::*, px,
};
use keystone_core::integrations::copy::{self, Drift, Pair, Preview};
use keystone_core::integrations::edit::{self, Change, Failure, Server, Target};
use keystone_core::integrations::{
    self, Detail, Health, Inventory, Item, Kind, Location, Scope, State, Transport,
};
use keystone_core::{AccountId, Provider};
use keystone_ui::prelude::*;
use keystone_ui::text_input::{InputMode, TextInput};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Numbers each read, so only the newest one fills the page that asked for it.
static READS: AtomicU64 = AtomicU64::new(0);
/// Numbers each copy preview, so only the newest one fills the panel.
static PREVIEWS: AtomicU64 = AtomicU64::new(0);

const KINDS: [(Kind, &str, &str); 3] = [
    (Kind::McpServer, "MCP servers", "mcp"),
    (Kind::Skill, "Skills", "skill"),
    (Kind::Plugin, "Plugins", "plugin"),
];

/// An environment value the edit form does not show, kept as it is.
const KEPT: &str = "\u{2022}\u{2022}\u{2022}\u{2022}";

/// One account's inventory, or `None` when its folder is unknown.
pub(crate) struct AccountInventory {
    provider: Provider,
    account: Option<AccountId>,
    label: String,
    location: Option<Location>,
    inventory: Option<Inventory>,
}

impl AccountInventory {
    fn id(&self) -> AccountId {
        self.account
            .clone()
            .unwrap_or_else(|| AccountId::new(AccountId::DEFAULT))
    }

    fn is(&self, provider: Provider, account: &AccountId) -> bool {
        self.provider == provider && self.id() == *account
    }
}

/// What the last change says, above the lists.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Notice {
    /// A file changed since it was read, so nothing was written.
    Conflict,
    Failed(String),
    /// Done while sessions of the account were running.
    NextStart,
}

#[derive(Clone, Debug)]
enum FormKind {
    AddServer,
    /// The server as its file has it, secrets included, for the fields left hidden.
    EditServer(Box<(Item, Server)>),
    AddSkill,
    Copy(Box<CopyForm>),
}

/// A copy being previewed: the item, the account in the read it comes from, the
/// secrets the user included, and what the copy would write, worked out for the
/// preview numbered `asked` or an earlier one.
#[derive(Clone, Debug)]
struct CopyForm {
    item: Item,
    from: usize,
    include: Vec<String>,
    asked: u64,
    preview: Option<(u64, Result<Preview, String>)>,
    /// The last press of Copy found the source changed since the preview it showed, and
    /// wrote nothing.
    changed: bool,
}

/// A copy to make: the preview numbered `asked` that the panel showed, and that preview
/// worked out again from the source as it is when the copy is made.
struct Copying {
    asked: u64,
    preview: Preview,
    again: Box<dyn FnOnce() -> Result<Preview, String> + Send>,
}

/// The open form: what it makes, and the account (an index into the read) and scope
/// it makes it for.
struct Form {
    kind: FormKind,
    account: usize,
    scope: usize,
}

pub(crate) struct IntegrationsPage {
    /// What the last read found, in the order Settings → Accounts lists accounts.
    read: Option<Vec<AccountInventory>>,
    reading: bool,
    /// The read this page waits for; an older one that finishes later is dropped.
    ticket: u64,
    refresh: FocusHandle,
    /// A change is being made; every control that makes one waits for it.
    applying: bool,
    pub(crate) notice: Option<Notice>,
    form: Option<Form>,
    /// Each copy Keystone made, as its two ends compared at the last read.
    copies: Vec<(Pair, Drift)>,
    name: Entity<TextInput>,
    target: Entity<TextInput>,
    args: Entity<TextInput>,
    env: Entity<TextInput>,
    folder: Entity<TextInput>,
    /// Focus for each control, made as it first renders.
    stops: RefCell<HashMap<String, FocusHandle>>,
}

fn field(placeholder: &str, cx: &mut App) -> Entity<TextInput> {
    let input = cx.new(|cx| TextInput::new(placeholder, InputMode::SingleLine, cx));
    let _ = input.read(cx).focus_handle(cx).tab_stop(true);
    input
}

impl IntegrationsPage {
    pub(crate) fn new(cx: &mut App) -> Self {
        Self {
            read: None,
            reading: false,
            ticket: 0,
            refresh: cx.focus_handle().tab_stop(true),
            applying: false,
            notice: None,
            form: None,
            copies: Vec::new(),
            name: field("Name", cx),
            target: field("Command or URL", cx),
            args: field("Arguments", cx),
            env: field("Environment, NAME=value", cx),
            folder: field("Skill folder", cx),
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

/// The scopes a form offers for `provider`.
fn scopes(provider: Provider, kind: &FormKind) -> &'static [Scope] {
    let server = match kind {
        FormKind::AddServer => true,
        FormKind::Copy(copy) => copy.item.kind() == Kind::McpServer,
        _ => false,
    };
    match provider {
        Provider::Claude if server => &[Scope::User, Scope::Local, Scope::Project],
        _ => &[Scope::User, Scope::Project],
    }
}

/// A server name both providers accept.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The form's fields as a server: a URL or a command, arguments as shell words, and
/// `NAME=value` words. A field or value left hidden keeps what `existing` has.
fn parse_server(
    target: &str,
    args: &str,
    env: &str,
    existing: Option<&Server>,
) -> Result<Server, String> {
    let kept = |text: &str| text.trim() == KEPT;
    let (old_target, old_args) = match existing.map(|server| &server.target) {
        Some(Target::Command { command, args }) => (command.as_str(), Some(args.clone())),
        Some(Target::Url(url)) => (url.as_str(), None),
        None => ("", None),
    };
    let target = if kept(target) {
        old_target
    } else {
        target.trim()
    };
    let args = match old_args {
        Some(old) if kept(args) => old,
        _ => shlex::split(args).ok_or("Close the quote in Arguments.")?,
    };
    let words = shlex::split(env).ok_or("Close the quote in Environment.")?;
    let env = words
        .into_iter()
        .map(|word| match word.split_once('=') {
            Some((name, value)) if valid_env_name(name) => {
                Ok((name.to_owned(), (value != KEPT).then(|| value.to_owned())))
            }
            _ => Err("Write each variable as NAME=value.".to_owned()),
        })
        .collect::<Result<_, _>>()?;
    let remote = ["http://", "https://", "ws://", "wss://"]
        .iter()
        .any(|scheme| target.starts_with(scheme));
    let target = match target {
        "" => return Err("Give a command or URL.".into()),
        url if remote && args.is_empty() => Target::Url(url.to_owned()),
        _ if remote => return Err("A URL takes no arguments.".into()),
        command => Target::Command {
            command: command.to_owned(),
            args,
        },
    };
    Ok(Server { target, env })
}

fn valid_env_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The edit form's text for `server`, hiding what the list hides: a URL, the
/// arguments, and a command with more than one word.
fn server_fields(server: &Server) -> (String, String, String) {
    let (target, args) = match &server.target {
        Target::Command { command, args } => (
            if command.split_whitespace().count() > 1 {
                KEPT.to_owned()
            } else {
                command.clone()
            },
            if args.is_empty() {
                String::new()
            } else {
                KEPT.to_owned()
            },
        ),
        Target::Url(_) => (KEPT.to_owned(), String::new()),
    };
    let env = server
        .env
        .iter()
        .map(|(name, _)| format!("{name}={KEPT}"))
        .collect::<Vec<_>>()
        .join(" ");
    (target, args, env)
}

/// Whether two items are one declaration.
fn same_item(a: &Item, b: &Item) -> bool {
    (
        a.provider,
        &a.account,
        a.kind(),
        a.scope,
        &a.source,
        &a.name,
    ) == (
        b.provider,
        &b.account,
        b.kind(),
        b.scope,
        &b.source,
        &b.name,
    )
}

/// `~/…` as a folder under `home`.
fn expand_home(path: &str, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
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
    if item.kind() == Kind::Plugin {
        return Some(copy::only(item.provider).into());
    }
    match item.state {
        State::On => None,
        // A server's or plugin's switch already shows it.
        State::Off if item.kind() != Kind::Skill => None,
        State::Off => Some("Off".into()),
        State::Untrusted => Some("Project not trusted by Codex".into()),
    }
}

/// What a copy says beside an item's name, ahead of [`note`]: that it and its other end
/// differ, or, on the copy, the secrets it still needs.
fn copy_note(item: &Item, copies: &[(Pair, Drift)], read: &[AccountInventory]) -> Option<String> {
    copies.iter().find_map(|(pair, drift)| {
        let other = if pair.from.is(item) {
            &pair.to
        } else if pair.to.is(item) {
            &pair.from
        } else {
            return None;
        };
        match drift {
            Drift::Differs => {
                let label = read
                    .iter()
                    .find(|account| account.is(other.provider, &other.account))
                    .map_or(other.account.as_str(), |account| account.label.as_str());
                Some(format!(
                    "Out of sync with {} · {label}",
                    provider_name(other.provider)
                ))
            }
            Drift::Needs(needs) if pair.to.is(item) => Some(format!("Needs {}", needs.join(", "))),
            _ => None,
        }
    })
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
        let database = state.store().database();
        let read = cx.background_executor().spawn(async move {
            let read = accounts
                .into_iter()
                .map(|(provider, account, label, home)| {
                    let location = env.location(provider, home.as_deref());
                    let inventory =
                        project
                            .as_deref()
                            .zip(location.as_ref())
                            .map(|(project, location)| {
                                let id = account
                                    .clone()
                                    .unwrap_or_else(|| AccountId::new(AccountId::DEFAULT));
                                integrations::read(&id, location, project)
                            });
                    AccountInventory {
                        provider,
                        account,
                        label,
                        location,
                        inventory,
                    }
                })
                .collect::<Vec<_>>();
            let location = |end: &copy::End| {
                read.iter()
                    .find(|account| account.is(end.provider, &end.account))
                    .and_then(|account| account.location.as_ref())
            };
            let copies = database
                .copies()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|pair| {
                    let (from, to) = (location(&pair.from)?, location(&pair.to)?);
                    let drift = copy::drift(&pair, from, to, project.as_deref()?)?;
                    Some((pair, drift))
                })
                .collect::<Vec<_>>();
            (read, copies)
        });
        cx.spawn(async move |this, cx| {
            let (read, copies) = read.await;
            let _ = this.update(cx, |this, cx| {
                let database = this.state.read(cx).store().database();
                let Some(page) = this
                    .integrations_page()
                    .filter(|page| page.ticket == ticket)
                else {
                    return;
                };
                // A form holds indexes into the read, so it closes when the accounts move.
                let accounts = |read: &[AccountInventory]| {
                    read.iter()
                        .map(|account| (account.provider, account.account.clone()))
                        .collect::<Vec<_>>()
                };
                if page
                    .read
                    .as_deref()
                    .is_some_and(|old| accounts(old) != accounts(&read))
                {
                    page.form = None;
                }
                page.read = Some(read);
                let (gone, copies): (Vec<_>, Vec<_>) = copies
                    .into_iter()
                    .partition(|(_, drift)| *drift == Drift::Gone);
                page.copies = copies;
                page.reading = false;
                cx.background_spawn(async move {
                    for (pair, _) in gone {
                        let _ = database.forget_copy(&pair);
                    }
                })
                .detach();
                this.refresh_copy(cx);
                cx.notify();
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
        let home = self.state.read(cx).config_env.home.clone();
        let project = self.presentation.project.as_ref().ok().map(|p| p.path());
        let now = crate::now_ms();
        // During a conflict, Review takes Refresh's place: they do the same.
        let conflict = integrations.notice == Some(Notice::Conflict);
        let top = h_flex()
            .gap(spacing::MD)
            .when(!conflict, |d| {
                d.child(
                    Button::new("integrations-refresh", "Refresh")
                        .style(ButtonStyle::Subtle)
                        .size(ButtonSize::Compact)
                        .disabled(integrations.reading || integrations.applying)
                        .track_focus(&integrations.refresh)
                        .debug_selector(|| "integrations-refresh".into())
                        .on_click(
                            self.scoped_listener(cx, |this, _, _, cx| this.review_integrations(cx)),
                        ),
                )
            })
            .children(self.integrations_notice(integrations, cx));
        let Some(read) = &integrations.read else {
            return v_flex()
                .automation_id("settings-integrations")
                .child(top)
                .child(
                    div().pt(spacing::LG).child(
                        Label::new("Reading…")
                            .size(TextSize::Ui)
                            .color(Color::Faint),
                    ),
                )
                .into_any_element();
        };
        let reports: Vec<_> = {
            let state = self.state.read(cx);
            read.iter()
                .map(|account| {
                    project
                        .map(|project| {
                            state.mcp_status(account.provider, account.account.as_ref(), project)
                        })
                        .unwrap_or_default()
                })
                .collect()
        };
        let editing = match &integrations.form {
            Some(Form {
                kind: FormKind::EditServer(editing),
                ..
            }) => Some(&editing.0),
            _ => None,
        };
        let mut rows: BTreeMap<Kind, Vec<AnyElement>> = BTreeMap::new();
        let mut problems = Vec::new();
        for (at, account) in read.iter().enumerate() {
            let who = format!("{} · {}", provider_name(account.provider), account.label);
            let Some(inventory) = &account.inventory else {
                problems.push(format!("{who}: folder unknown"));
                continue;
            };
            let reported = &reports[at];
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
                if editing.is_some_and(|editing| same_item(editing, item)) {
                    let form = self.server_form(integrations, read, cx);
                    rows.entry(item.kind()).or_default().push(form);
                    continue;
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
                let actions = self.item_actions(at, item, &id, integrations, cx);
                let copying = match &integrations.form {
                    Some(Form {
                        kind: FormKind::Copy(copy),
                        ..
                    }) => copy.from == at && same_item(&copy.item, item),
                    _ => false,
                };
                let note = copy_note(item, &integrations.copies, read).or_else(|| note(item));
                let rows = rows.entry(item.kind()).or_default();
                rows.push(Self::integration_row(
                    &id,
                    &item.name,
                    note,
                    (detail(item), item.kind() == Kind::McpServer),
                    meta,
                    status.into_iter().chain(Some(actions)),
                    cx,
                ));
                if copying {
                    rows.push(self.copy_form(integrations, read, home.as_deref(), cx));
                }
            }
            for (name, status) in reported {
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
                        (String::new(), false),
                        who.clone(),
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
            let adds = match kind {
                Kind::McpServer => Some((FormKind::AddServer, "Add server")),
                Kind::Skill => Some((FormKind::AddSkill, "Add skill folder")),
                Kind::Plugin => None,
            };
            let form = match (&integrations.form, kind) {
                (
                    Some(Form {
                        kind: FormKind::AddServer,
                        ..
                    }),
                    Kind::McpServer,
                ) => Some(self.server_form(integrations, read, cx)),
                (
                    Some(Form {
                        kind: FormKind::AddSkill,
                        ..
                    }),
                    Kind::Skill,
                ) => Some(self.skill_form(integrations, read, cx)),
                _ => None,
            };
            let add = adds.map(|(form, tooltip)| {
                let id = format!("integrations-add-{slug}");
                IconButton::new(id.clone(), IconName::Plus)
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Compact)
                    .track_focus(&integrations.stop(&id, cx))
                    .debug_selector(move || id.clone())
                    .aria_label(tooltip)
                    .tooltip(Tooltip::text(tooltip))
                    .on_click(self.scoped_listener(cx, move |this, _, window, cx| {
                        this.open_integration_form(form.clone(), window, cx)
                    }))
            });
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
                        )
                        .child(div().flex_1())
                        .children(add),
                )
                .children(form)
                .children(rows)
        });
        v_flex()
            .automation_id("settings-integrations")
            .child(top)
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

    /// What the last change says, at the top of the section.
    fn integrations_notice(
        &self,
        page: &IntegrationsPage,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let line = |id: &str, text: String, color| {
            div()
                .automation_id(id.to_owned())
                .child(Label::new(text).size(TextSize::Sm).color(color))
        };
        Some(match page.notice.as_ref()? {
            Notice::Conflict => h_flex()
                .gap_2()
                .automation_id("integrations-conflict")
                .child(
                    Label::new("Edited outside Keystone")
                        .size(TextSize::Sm)
                        .color(Color::Warning),
                )
                .child(
                    Button::new("integrations-review", "Review")
                        .style(ButtonStyle::Subtle)
                        .size(ButtonSize::Compact)
                        .track_focus(&page.stop("review", cx))
                        .debug_selector(|| "integrations-review".into())
                        .on_click(
                            self.scoped_listener(cx, |this, _, _, cx| this.review_integrations(cx)),
                        ),
                )
                .into_any_element(),
            Notice::Failed(error) => {
                line("integrations-failed", error.clone(), Color::Warning).into_any_element()
            }
            Notice::NextStart => line(
                "integrations-next-start",
                "Running sessions get this at their next start.".into(),
                Color::Faint,
            )
            .into_any_element(),
        })
    }

    /// A row's switch, edit and remove controls, for what Keystone can change.
    fn item_actions(
        &self,
        at: usize,
        item: &Item,
        id: &str,
        page: &IntegrationsPage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let plugin_skill = matches!(
            item.detail,
            Detail::Skill {
                plugin: Some(_),
                ..
            }
        );
        let icon = |suffix: &str, icon, tooltip: &'static str| {
            let id = format!("{id}-{suffix}");
            IconButton::new(id.clone(), icon)
                .style(ButtonStyle::Subtle)
                .size(ButtonSize::Compact)
                .track_focus(&page.stop(&id, cx))
                .debug_selector(move || id.clone())
                .aria_label(tooltip)
                .tooltip(Tooltip::text(tooltip))
        };
        let mut actions = h_flex().flex_none().gap_1().items_center();
        let default = page
            .read
            .as_ref()
            .and_then(|read| read.get(at))
            .is_some_and(|account| account.account.is_none());
        // Only the default Claude account's own .claude.json holds this switch.
        let only_claude = default
            && item.provider == Provider::Claude
            && item.kind() == Kind::McpServer
            && item.scope != Scope::Project;
        if only_claude {
            actions = actions.child(
                div()
                    .id(format!("{id}-switch-claude"))
                    .automation_id(format!("{id}-switch-claude"))
                    .tooltip(Tooltip::text("Use /mcp in Claude.").builder())
                    .child(
                        Switch::new(format!("{id}-switch"), item.state == State::On).disabled(true),
                    ),
            );
        } else if item.kind() != Kind::Skill && item.state != State::Untrusted {
            let target = item.clone();
            actions = actions.child(
                Switch::new(format!("{id}-switch"), item.state == State::On).on_change(
                    self.scoped_listener(cx, move |this, &on: &bool, _, cx| {
                        let item = target.clone();
                        this.change_integration(at, Change::Switch { item, on }, None, cx)
                    }),
                ),
            );
        }
        if item.kind() == Kind::McpServer {
            let target = item.clone();
            actions = actions.child(icon("edit", IconName::Pencil, "Edit").on_click(
                self.scoped_listener(cx, move |this, _, window, cx| {
                    this.edit_integration_server(at, &target, window, cx)
                }),
            ));
        }
        if item.kind() != Kind::Plugin && !plugin_skill {
            let target = item.clone();
            actions = actions.child(icon("copy", IconName::Copy, "Copy to…").on_click(
                self.scoped_listener(cx, move |this, _, _, cx| this.open_copy(at, &target, cx)),
            ));
            let target = item.clone();
            actions = actions.child(icon("remove", IconName::Close, "Remove").on_click(
                self.scoped_listener(cx, move |this, _, _, cx| {
                    let item = target.clone();
                    this.change_integration(at, Change::Remove { item }, None, cx)
                }),
            ));
        }
        actions.into_any_element()
    }

    /// The accounts a form can choose, as indexes into the read and their labels.
    fn form_accounts(read: &[AccountInventory]) -> Vec<(usize, String)> {
        read.iter()
            .enumerate()
            .filter(|(_, account)| account.location.is_some() && account.inventory.is_some())
            .map(|(at, account)| {
                let name = provider_name(account.provider);
                (at, format!("{name} · {}", account.label))
            })
            .collect()
    }

    /// The account and scope choices at the head of an add form.
    fn form_target(
        &self,
        page: &IntegrationsPage,
        read: &[AccountInventory],
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let form = page.form.as_ref().expect("a form is open");
        let mut accounts = Self::form_accounts(read);
        if let FormKind::Copy(copy) = &form.kind {
            accounts.retain(|(at, _)| *at != copy.from);
        }
        let provider = read[form.account].provider;
        let scopes = scopes(provider, &form.kind);
        let choices: Vec<usize> = accounts.iter().map(|(at, _)| *at).collect();
        h_flex()
            .flex_wrap()
            .gap(spacing::SM)
            .child(
                div().flex_none().child(
                    Select::new(
                        "integrations-form-account",
                        accounts.iter().map(|(_, label)| label.clone()),
                        choices.iter().position(|at| *at == form.account),
                    )
                    .width(px(200.))
                    .track_focus(&page.stop("form-account", cx))
                    .on_change(self.scoped_listener(
                        cx,
                        move |this, &chosen: &usize, _, cx| {
                            if let Some(form) = this.integrations_form() {
                                form.account = choices[chosen];
                                form.scope = 0;
                                if let FormKind::Copy(copy) = &mut form.kind {
                                    copy.changed = false;
                                }
                            }
                            this.refresh_copy(cx);
                            cx.notify();
                        },
                    )),
                ),
            )
            .child(
                div().flex_none().child(
                    Select::new(
                        "integrations-form-scope",
                        scopes.iter().map(|scope| scope.label()),
                        Some(form.scope),
                    )
                    .width(px(120.))
                    .track_focus(&page.stop("form-scope", cx))
                    .on_change(self.scoped_listener(
                        cx,
                        |this, &scope: &usize, _, cx| {
                            if let Some(form) = this.integrations_form() {
                                form.scope = scope;
                                if let FormKind::Copy(copy) = &mut form.kind {
                                    copy.changed = false;
                                }
                            }
                            this.refresh_copy(cx);
                            cx.notify();
                        },
                    )),
                ),
            )
    }

    fn form_input(
        entity: &Entity<TextInput>,
        id: &'static str,
        cx: &App,
    ) -> impl IntoElement + use<> {
        let colors = cx.theme().colors;
        div()
            .flex_1()
            .min_w(px(160.))
            .px(px(8.))
            .rounded(px(6.))
            .border_1()
            .border_color(colors.border)
            .bg(colors.background)
            .automation_id(id)
            .child(entity.clone())
    }

    fn form_buttons(
        &self,
        page: &IntegrationsPage,
        submit: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        h_flex()
            .gap(spacing::SM)
            .justify_end()
            .child(
                Button::new("integrations-form-cancel", "Cancel")
                    .style(ButtonStyle::Subtle)
                    .track_focus(&page.stop("form-cancel", cx))
                    .debug_selector(|| "integrations-form-cancel".into())
                    .on_click(
                        self.scoped_listener(cx, |this, _, _, cx| this.close_integration_form(cx)),
                    ),
            )
            .child(
                Button::new("integrations-form-submit", submit)
                    .style(ButtonStyle::Filled)
                    .track_focus(&page.stop("form-submit", cx))
                    .debug_selector(|| "integrations-form-submit".into())
                    .on_click(
                        self.scoped_listener(cx, |this, _, _, cx| this.submit_integration_form(cx)),
                    ),
            )
    }

    fn form_keys(
        &self,
        cx: &mut Context<Self>,
    ) -> impl Fn(&KeyDownEvent, &mut Window, &mut App) + 'static {
        self.scoped_listener(cx, |this, event: &KeyDownEvent, _, cx| {
            match event.keystroke.key.as_str() {
                "enter" => this.submit_integration_form(cx),
                "escape" => this.close_integration_form(cx),
                _ => return,
            }
            cx.stop_propagation();
        })
    }

    /// The add or edit form for an MCP server.
    fn server_form(
        &self,
        page: &IntegrationsPage,
        read: &[AccountInventory],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let form = page.form.as_ref().expect("a form is open");
        let head: AnyElement = match &form.kind {
            FormKind::EditServer(editing) => Label::new(editing.0.name.clone())
                .truncate()
                .into_any_element(),
            _ => h_flex()
                .flex_wrap()
                .gap(spacing::SM)
                .child(self.form_target(page, read, cx))
                .child(Self::form_input(&page.name, "integrations-form-name", cx))
                .into_any_element(),
        };
        let submit = match form.kind {
            FormKind::EditServer(..) => "Save",
            _ => "Add",
        };
        v_flex()
            .automation_id("integrations-server-form")
            .gap(spacing::SM)
            .py(spacing::SM)
            .hairline_t(cx)
            .on_key_down(self.form_keys(cx))
            .child(head)
            .child(Self::form_input(
                &page.target,
                "integrations-form-target",
                cx,
            ))
            .child(Self::form_input(&page.args, "integrations-form-args", cx))
            .child(Self::form_input(&page.env, "integrations-form-env", cx))
            .child(self.form_buttons(page, submit, cx))
            .into_any_element()
    }

    fn skill_form(
        &self,
        page: &IntegrationsPage,
        read: &[AccountInventory],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        v_flex()
            .automation_id("integrations-skill-form")
            .gap(spacing::SM)
            .py(spacing::SM)
            .hairline_t(cx)
            .on_key_down(self.form_keys(cx))
            .child(
                h_flex()
                    .flex_wrap()
                    .gap(spacing::SM)
                    .child(self.form_target(page, read, cx))
                    .child(Self::form_input(
                        &page.folder,
                        "integrations-form-folder",
                        cx,
                    )),
            )
            .child(self.form_buttons(page, "Add", cx))
            .into_any_element()
    }

    /// The copy panel under the row being copied: where to, what it writes there, and
    /// a switch for each secret.
    fn copy_form(
        &self,
        page: &IntegrationsPage,
        read: &[AccountInventory],
        home: Option<&Path>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(Form {
            kind: FormKind::Copy(copy),
            ..
        }) = &page.form
        else {
            return div().into_any_element();
        };
        let body = match &copy.preview {
            None => div().into_any_element(),
            Some((_, Ok(preview))) => v_flex()
                .gap(spacing::SM)
                .children(copy.changed.then(|| {
                    div().automation_id("integrations-copy-changed").child(
                        Label::new(format!("{} changed since the preview.", copy.item.name))
                            .size(TextSize::Sm)
                            .color(Color::Warning),
                    )
                }))
                .child(
                    Label::new(shown_path(&preview.to, home))
                        .size(TextSize::Sm)
                        .color(Color::Muted)
                        .truncate(),
                )
                .child(
                    v_flex()
                        .automation_id("integrations-copy-preview")
                        .children(
                            preview
                                .text
                                .lines()
                                .map(|line| Label::new(line.to_owned()).size(TextSize::Sm).mono()),
                        ),
                )
                .children((!preview.secrets.is_empty()).then(|| {
                    h_flex()
                        .flex_wrap()
                        .gap(spacing::MD)
                        .children(preview.secrets.iter().map(|secret| {
                            let name = secret.clone();
                            Switch::new(
                                format!("integrations-copy-secret-{secret}"),
                                copy.include.contains(secret),
                            )
                            .label(secret.clone())
                            .on_change(self.scoped_listener(cx, move |this, &on: &bool, _, cx| {
                                if let Some(Form {
                                    kind: FormKind::Copy(copy),
                                    ..
                                }) = this.integrations_form()
                                {
                                    copy.include.retain(|included| *included != name);
                                    if on {
                                        copy.include.push(name.clone());
                                    }
                                }
                                this.refresh_copy(cx);
                                cx.notify();
                            }))
                        }))
                }))
                .into_any_element(),
            Some((_, Err(error))) => div()
                .automation_id("integrations-copy-error")
                .child(
                    Label::new(error.clone())
                        .size(TextSize::Sm)
                        .color(Color::Warning),
                )
                .into_any_element(),
        };
        v_flex()
            .automation_id("integrations-copy-form")
            .gap(spacing::SM)
            .py(spacing::SM)
            .hairline_t(cx)
            .on_key_down(self.form_keys(cx))
            .child(self.form_target(page, read, cx))
            .child(body)
            .child(self.form_buttons(page, "Copy", cx))
            .into_any_element()
    }

    /// Opens the copy panel on `item`, from the account at `at`, to the first other
    /// account of the other provider, or else of the same one.
    fn open_copy(&mut self, at: usize, item: &Item, cx: &mut Context<Self>) {
        let Some(page) = self.integrations_page() else {
            return;
        };
        let Some(read) = &page.read else { return };
        let accounts = Self::form_accounts(read);
        let target = accounts
            .iter()
            .find(|(to, _)| read[*to].provider != item.provider)
            .or_else(|| accounts.iter().find(|(to, _)| *to != at));
        let Some(&(account, _)) = target else {
            return;
        };
        let kind = FormKind::Copy(Box::new(CopyForm {
            item: item.clone(),
            from: at,
            include: Vec::new(),
            asked: 0,
            preview: None,
            changed: false,
        }));
        let scope = scopes(read[account].provider, &kind)
            .iter()
            .position(|scope| *scope == item.scope)
            .unwrap_or(0);
        page.notice = None;
        page.form = Some(Form {
            kind,
            account,
            scope,
        });
        self.refresh_copy(cx);
        cx.notify();
    }

    /// What the open copy would write, worked out by the closure from the item, accounts,
    /// scope and secrets the panel names now; `None` when no copy panel is open or a
    /// folder it needs is unknown.
    fn copy_preview(&self) -> Option<impl FnOnce() -> Result<Preview, String> + Send + use<>> {
        let project = self.presentation.project.as_ref().ok()?.path().to_owned();
        let page = &self.settings.as_ref()?.integrations;
        let (form, read) = (page.form.as_ref()?, page.read.as_ref()?);
        let to = read.get(form.account)?;
        let scope = *scopes(to.provider, &form.kind).get(form.scope)?;
        let FormKind::Copy(copy) = &form.kind else {
            return None;
        };
        let from = read.get(copy.from)?.location.clone()?;
        let (location, account) = (to.location.clone()?, to.id());
        let (item, include) = (copy.item.clone(), copy.include.clone());
        Some(move || {
            copy::preview(
                &item,
                &from,
                (&location, &account, scope),
                &project,
                &include,
            )
        })
    }

    /// Works out again, off the UI thread, what the open copy would write.
    fn refresh_copy(&mut self, cx: &mut Context<Self>) {
        let Some(preview) = self.copy_preview() else {
            return;
        };
        let Some(Form {
            kind: FormKind::Copy(copy),
            ..
        }) = self.integrations_form()
        else {
            return;
        };
        let asked = PREVIEWS.fetch_add(1, Ordering::Relaxed) + 1;
        copy.asked = asked;
        let task = cx.background_executor().spawn(async move { preview() });
        cx.spawn(async move |this, cx| {
            let preview = task.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(Form {
                    kind: FormKind::Copy(copy),
                    ..
                }) = this.integrations_form()
                    && copy.asked == asked
                {
                    copy.preview = Some((asked, preview));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn integrations_page(&mut self) -> Option<&mut IntegrationsPage> {
        self.settings.as_mut().map(|page| &mut page.integrations)
    }

    fn integrations_form(&mut self) -> Option<&mut Form> {
        self.integrations_page()?.form.as_mut()
    }

    /// Clears the last notice and reads every file again.
    fn review_integrations(&mut self, cx: &mut Context<Self>) {
        if let Some(page) = self.integrations_page() {
            page.notice = None;
        }
        self.read_integrations(cx);
    }

    /// Opens an empty add form for the first account it can use.
    fn open_integration_form(
        &mut self,
        kind: FormKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(page) = self.integrations_page() else {
            return;
        };
        let Some(read) = &page.read else { return };
        let Some(&(account, _)) = Self::form_accounts(read).first() else {
            return;
        };
        let first = match kind {
            FormKind::AddSkill => page.folder.clone(),
            _ => page.name.clone(),
        };
        let fields = [
            &page.name,
            &page.target,
            &page.args,
            &page.env,
            &page.folder,
        ]
        .map(Entity::clone);
        page.form = Some(Form {
            kind,
            account,
            scope: 0,
        });
        for field in fields {
            field.update(cx, |input, cx| input.set_value(String::new(), cx));
        }
        first.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    /// Opens the edit form on `item`, filled from its file.
    fn edit_integration_server(
        &mut self,
        at: usize,
        item: &Item,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let project = self
            .presentation
            .project
            .as_ref()
            .ok()
            .map(|p| p.path().to_owned());
        let Some(page) = self.integrations_page() else {
            return;
        };
        let location = page
            .read
            .as_ref()
            .and_then(|read| read.get(at))
            .and_then(|account| account.location.clone());
        let server = location
            .zip(project)
            .and_then(|(location, project)| edit::server(&location, &project, item));
        let Some(server) = server else {
            page.notice = Some(Notice::Failed(format!("{} is no longer there.", item.name)));
            cx.notify();
            return;
        };
        let (target, args, env) = server_fields(&server);
        page.notice = None;
        page.form = Some(Form {
            kind: FormKind::EditServer(Box::new((item.clone(), server))),
            account: at,
            scope: 0,
        });
        let fields = [
            (page.target.clone(), target),
            (page.args.clone(), args),
            (page.env.clone(), env),
        ];
        let focus = page.target.clone();
        for (field, value) in fields {
            field.update(cx, |input, cx| input.set_value(value, cx));
        }
        focus.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn close_integration_form(&mut self, cx: &mut Context<Self>) {
        if let Some(page) = self.integrations_page() {
            page.form = None;
        }
        cx.notify();
    }

    /// Makes the change the open form describes.
    fn submit_integration_form(&mut self, cx: &mut Context<Self>) {
        let home = self.state.read(cx).config_env.home.clone();
        let Some(page) = self.settings.as_ref().map(|page| &page.integrations) else {
            return;
        };
        let (Some(form), Some(read)) = (&page.form, &page.read) else {
            return;
        };
        if let FormKind::Copy(copy) = &form.kind {
            // Only the preview of what the panel shows now, for the account it names.
            if let Some((asked, Ok(preview))) = copy.preview.clone()
                && asked == copy.asked
                && read
                    .get(form.account)
                    .is_some_and(|to| to.is(preview.pair.to.provider, &preview.pair.to.account))
                && let Some(again) = self.copy_preview()
            {
                let at = form.account;
                let change = preview.change.clone();
                let copying = Copying {
                    asked,
                    preview,
                    again: Box::new(again),
                };
                self.change_integration(at, change, Some(copying), cx);
            }
            return;
        }
        let value = |input: &Entity<TextInput>| input.read(cx).value().trim().to_owned();
        let provider = read[form.account].provider;
        let scope = scopes(provider, &form.kind).get(form.scope).copied();
        let existing = match &form.kind {
            FormKind::EditServer(editing) => Some(&editing.1),
            _ => None,
        };
        let server = || {
            parse_server(
                &value(&page.target),
                &value(&page.args),
                &value(&page.env),
                existing,
            )
        };
        let change = match (&form.kind, scope) {
            (FormKind::EditServer(editing), _) => server().map(|server| Change::Edit {
                item: editing.0.clone(),
                server,
            }),
            (FormKind::AddServer, Some(scope)) => {
                let name = value(&page.name);
                if valid_name(&name) {
                    server().map(|server| Change::AddServer {
                        scope,
                        name,
                        server,
                    })
                } else {
                    Err("Name it with letters, digits, - and _.".into())
                }
            }
            (FormKind::AddSkill, Some(scope)) => Ok(Change::AddSkill {
                scope,
                folder: expand_home(&value(&page.folder), home.as_deref()),
                content: None,
            }),
            (FormKind::Copy(_), _) | (_, None) => return,
        };
        let at = form.account;
        match change {
            Ok(change) => self.change_integration(at, change, None, cx),
            Err(error) => {
                if let Some(page) = self.integrations_page() {
                    page.notice = Some(Notice::Failed(error));
                }
                cx.notify();
            }
        }
    }

    /// Makes `change` to the account at `at` in the read off the UI thread, against the
    /// files as that read found them, then reads them again. A copy's change is its
    /// preview's, and it is made only while the preview worked out again just before
    /// is still that one; otherwise nothing is written and, while the panel is still that
    /// copy, it shows the new preview. What a copy writes is always what its preview
    /// showed: a server's definition is in the change, and a skill folder's copy lands
    /// only if it still has the content the preview hashed. A source edited after the
    /// check shows later as out of sync. A made copy is remembered.
    fn change_integration(
        &mut self,
        at: usize,
        change: Change,
        copying: Option<Copying>,
        cx: &mut Context<Self>,
    ) {
        let project = self
            .presentation
            .project
            .as_ref()
            .ok()
            .map(|p| p.path().to_owned());
        let state = self.state.read(cx);
        let backups = state
            .store()
            .path()
            .parent()
            .unwrap_or(Path::new("."))
            .join("config-backups");
        let Some(page) = self.settings.as_ref().map(|page| &page.integrations) else {
            return;
        };
        if page.applying || page.reading {
            return;
        }
        let Some(account) = page.read.as_ref().and_then(|read| read.get(at)) else {
            return;
        };
        let (Some(location), Some(inventory), Some(project)) = (
            account.location.clone(),
            account.inventory.as_ref(),
            project,
        ) else {
            return;
        };
        let (provider, id) = (account.provider, account.account.clone());
        let shown = inventory.files.clone();
        let program = state.provider_program(provider);
        // A project's own files reach every account of the provider that opens it.
        let project_scope = match &change {
            Change::Switch { item, .. } | Change::Edit { item, .. } | Change::Remove { item } => {
                item.scope == Scope::Project
            }
            Change::AddServer { scope, .. }
            | Change::AddSkill { scope, .. }
            | Change::AddDefinition { scope, .. } => *scope == Scope::Project,
        };
        let running = state.has_running_session(provider, id.as_ref(), project_scope);
        let profile = state.profile(provider, id.as_ref());
        let claude_writes_global = id.is_none()
            || (provider == Provider::Claude
                && state.has_running_session(provider, id.as_ref(), false));
        if let Some(page) = self.integrations_page() {
            page.applying = true;
            page.notice = None;
            if let Some(Form {
                kind: FormKind::Copy(copy),
                ..
            }) = &mut page.form
                && copying.is_some()
            {
                copy.changed = false;
            }
        }
        let copied = copying
            .as_ref()
            .map(|copying| (copying.asked, copying.preview.pair.clone()));
        let database = self.state.read(cx).store().database();
        let task = cx.background_executor().spawn(async move {
            let mut pair = None;
            if let Some(copying) = copying {
                let again = (copying.again)();
                if again.as_ref() != Ok(&copying.preview) {
                    return (Err(again), Ok(()));
                }
                pair = Some(copying.preview.pair);
            }
            let made = profile.map_err(Failure::Failed).and_then(|profile| {
                let plan = edit::plan(&location, claude_writes_global, &project, change)
                    .map_err(Failure::Failed)?;
                edit::apply(plan, &shown, &backups, &mut |call| {
                    keystone_agent::run_config_cli(provider, program.as_deref(), &profile, call)
                })
            });
            let recorded = match (&made, pair) {
                (Ok(()), Some(pair)) => database.record_copy(&pair),
                _ => Ok(()),
            };
            (Ok(made), recorded)
        });
        cx.spawn(async move |this, cx| {
            let (made, recorded) = task.await;
            let _ = this.update(cx, |this, cx| {
                let Some(page) = this.integrations_page() else {
                    return;
                };
                page.applying = false;
                let result = match made {
                    Ok(result) => result,
                    Err(again) => {
                        if let Some(form) = &mut page.form
                            && let Some((asked, pair)) = &copied
                            && page
                                .read
                                .as_ref()
                                .and_then(|read| read.get(form.account))
                                .is_some_and(|to| {
                                    to.is(pair.to.provider, &pair.to.account)
                                        && scopes(to.provider, &form.kind).get(form.scope)
                                            == Some(&pair.to.scope)
                                })
                            && let FormKind::Copy(copy) = &mut form.kind
                            && pair.from.is(&copy.item)
                        {
                            copy.changed = again.is_ok();
                            if *asked == copy.asked {
                                copy.preview = Some((copy.asked, again));
                            }
                        }
                        this.read_integrations(cx);
                        cx.notify();
                        return;
                    }
                };
                page.notice = match &result {
                    Ok(()) => running.then_some(Notice::NextStart),
                    Err(Failure::Conflict(_)) => Some(Notice::Conflict),
                    Err(Failure::Failed(error)) => Some(Notice::Failed(error.clone())),
                };
                if let Err(error) = recorded {
                    page.notice = Some(Notice::Failed(error.to_string()));
                }
                if result.is_ok() {
                    page.form = None;
                }
                if !matches!(result, Err(Failure::Conflict(_))) {
                    this.read_integrations(cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn integration_row(
        id: &str,
        name: &str,
        note: Option<String>,
        (detail, mono): (String, bool),
        meta: String,
        trailing: impl IntoIterator<Item = AnyElement>,
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
                                div()
                                    .automation_id(format!("{id}-note"))
                                    .child(Label::new(note).size(TextSize::Sm).color(Color::Faint))
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
            .children(trailing)
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

    #[test]
    fn the_server_form_reads_shell_words_and_keeps_hidden_values() {
        let existing = Server {
            target: Target::Command {
                command: "npx".into(),
                args: vec!["--token".into(), "secret".into()],
            },
            env: vec![("B".into(), None)],
        };
        let (target, args, env) = server_fields(&existing);
        assert_eq!((target.as_str(), args.as_str()), ("npx", KEPT));
        assert_eq!(env, format!("B={KEPT}"));
        assert_eq!(
            parse_server(&target, &args, &env, Some(&existing)).unwrap(),
            existing,
            "hidden fields keep what the file has"
        );
        let url = Server {
            target: Target::Url("https://u:secret@m.example/mcp".into()),
            env: Vec::new(),
        };
        assert_eq!(server_fields(&url).0, KEPT);
        assert_eq!(parse_server(KEPT, "", "", Some(&url)).unwrap(), url);
        let server = parse_server("npx", "-y 'my server'", &format!("A=1 B={KEPT}"), None).unwrap();
        assert_eq!(
            server,
            Server {
                target: Target::Command {
                    command: "npx".into(),
                    args: vec!["-y".into(), "my server".into()],
                },
                env: vec![("A".into(), Some("1".into())), ("B".into(), None)],
            }
        );
        assert_eq!(
            parse_server("https://m.example/mcp", "", "", None)
                .unwrap()
                .target,
            Target::Url("https://m.example/mcp".into())
        );
        for (target, args, env, error) in [
            ("", "", "", "Give a command or URL."),
            ("https://m.example", "x", "", "A URL takes no arguments."),
            ("npx", "'open", "", "Close the quote in Arguments."),
            ("npx", "", "NOVALUE", "Write each variable as NAME=value."),
        ] {
            assert_eq!(parse_server(target, args, env, None).unwrap_err(), error);
        }
        assert!(valid_name("my-server_2"));
        assert!(!valid_name("my server"));
    }
}
