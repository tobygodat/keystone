//! The composer's `/` menu. A draft that starts with `/` and has no space yet lists the
//! commands and skills the session's account offers in its folder, from the same
//! request as the model catalog, so it works before the first Send. Each row names its
//! source. Choosing a row replaces the draft with its invocation and a space, and the
//! composer's hint shows what it takes. Built-ins that only the provider's own terminal
//! runs are marked Terminal only: choosing or sending one sends nothing and shows the
//! command that opens that terminal instead.
//!
//! Saved prompts this window's project offers join the rows with source Saved, by
//! shortcut or by title. Choosing one fills the composer with its text and never
//! sends. A command with the same name as a prompt's shortcut wins: it is listed
//! first, sending `/name` runs it, and the prompt's row says so.

use crate::{CONTENT_WIDTH, ComposerDraft, Keystone, NextFocus, presentation};
use gpui::{
    Anchor, AnyElement, App, ClipboardItem, Context, Entity, Focusable, FontWeight, HighlightStyle,
    IntoElement, Pixels, StyledText, Subscription, Window, anchored, deferred, div, prelude::*, px,
};
use keystone_agent::{CommandSource, Commands, SlashCommand, terminal_command, terminal_only};
use keystone_core::{AccountId, Prompt, Provider};
use keystone_ui::{fuzzy, prelude::*, text_input};

const VISIBLE_ROWS: usize = 8;

static NONE_LISTED: Commands = Commands::Ready(Vec::new());

/// The menu's own state, kept while the app runs.
pub(crate) struct SlashMenu {
    list: Entity<VirtualListState>,
    /// The session and draft Escape closed the menu on. It stays closed until an edit.
    dismissed: Option<(u64, String)>,
    notice: Option<Notice>,
    _edited: Subscription,
}

/// A Terminal only command the user chose or sent, shown until the draft, provider or
/// account changes.
struct Notice {
    session: u64,
    draft: String,
    invocation: String,
    provider: Provider,
    account: Option<AccountId>,
    copied: bool,
}

/// One row: a command or a saved prompt, and the bytes of its label the query
/// matched.
pub(crate) struct Entry {
    pub item: Item,
    positions: Vec<usize>,
}

pub(crate) enum Item {
    Command(SlashCommand),
    Saved(Saved),
}

/// A saved prompt as the menu lists it.
pub(crate) struct Saved {
    pub id: u64,
    /// `/shortcut`, or its title when it has none.
    label: String,
    /// Its title under a shortcut, or the first line of its text.
    description: String,
    /// A command of the same name, in any case, runs instead when `/shortcut` is
    /// sent.
    pub shadowed: bool,
    /// The account has not listed its commands yet, so one may still take
    /// `/shortcut`: Enter and Tab wait for the list, and a click inserts it.
    pending: bool,
}

impl Item {
    /// What the row shows first and the query matches, after any `/` or `$`.
    fn label(&self) -> &str {
        match self {
            Self::Command(command) => &command.invocation,
            Self::Saved(saved) => &saved.label,
        }
    }

    /// Where in [`Item::label`] the matched name starts.
    fn name_start(&self) -> usize {
        match self {
            Self::Command(_) => 1,
            Self::Saved(saved) => usize::from(saved.label.starts_with('/')),
        }
    }

    fn description(&self) -> &str {
        match self {
            Self::Command(command) => command.description.lines().next().unwrap_or_default(),
            Self::Saved(saved) => &saved.description,
        }
    }

    fn argument_hint(&self) -> &str {
        match self {
            Self::Command(command) => &command.argument_hint,
            Self::Saved(_) => "",
        }
    }

    /// A prompt a command wins over, or may once the account answers: listed after
    /// every command.
    fn deferred(&self) -> bool {
        matches!(self, Self::Saved(saved) if saved.shadowed || saved.pending)
    }

    fn terminal_only(&self) -> bool {
        matches!(self, Self::Command(command) if command.terminal_only)
    }
}

impl SlashMenu {
    /// Each edit of `composer` opens a closed menu and highlights its first row.
    pub(crate) fn new(composer: &Entity<TextInput>, cx: &mut Context<Keystone>) -> Self {
        Self {
            list: cx.new(|cx| VirtualListState::new(0, cx)),
            dismissed: None,
            notice: None,
            _edited: cx.subscribe(composer, |this, _, _: &text_input::Changed, cx| {
                this.slash.dismissed = None;
                this.slash.list.update(cx, |list, _| list.move_to(0));
            }),
        }
    }
}

/// What the menu filters by: the draft after its leading `/`, while it has no space.
fn query(draft: &str) -> Option<&str> {
    let rest = draft.strip_prefix('/')?;
    (!rest.contains(char::is_whitespace)).then_some(rest)
}

/// The rows for `query`: the saved `prompts`, what the account lists, the provider's
/// Terminal only built-ins it does not list, then the prompts a command shares a name
/// with, or may share one with while the account's list loads. An empty query keeps
/// that order; otherwise those deferred prompts still come last, and otherwise the
/// best match comes first, then the shorter name, then a command before a prompt.
pub(crate) fn entries(
    listed: Option<&Commands>,
    provider: Provider,
    query: &str,
    prompts: &[&Prompt],
) -> Vec<Entry> {
    let mut commands: Vec<SlashCommand> = match listed {
        Some(Commands::Ready(commands)) => commands.clone(),
        _ => Vec::new(),
    };
    let terminal: Vec<_> = terminal_only(provider)
        .into_iter()
        .filter(|t| !commands.iter().any(|c| c.invocation == t.invocation))
        .collect();
    commands.extend(terminal);
    let loading = listed.is_none();
    let saved = prompts.iter().map(|prompt| {
        let shortcut = prompt.shortcut().map(|s| format!("/{s}"));
        Item::Saved(Saved {
            id: prompt.id(),
            shadowed: shortcut.as_ref().is_some_and(|s| {
                commands
                    .iter()
                    .any(|c| c.invocation.eq_ignore_ascii_case(s))
            }),
            pending: loading && shortcut.is_some(),
            description: match &shortcut {
                Some(_) => prompt.title().to_owned(),
                None => prompt.body().lines().next().unwrap_or_default().to_owned(),
            },
            label: shortcut.unwrap_or_else(|| prompt.title().to_owned()),
        })
    });
    let (shadowed, saved): (Vec<_>, Vec<_>) =
        saved.partition(|item| matches!(item, Item::Saved(s) if s.shadowed || s.pending));
    let items: Vec<Item> = saved
        .into_iter()
        .chain(commands.into_iter().map(Item::Command))
        .chain(shadowed)
        .collect();
    let mut scored: Vec<(i32, Entry)> = items
        .into_iter()
        .filter_map(|item| {
            let start = item.name_start();
            let found = fuzzy::score(query, &item.label()[start..])?;
            let positions = found.positions.iter().map(|p| p + start).collect();
            Some((found.score, Entry { item, positions }))
        })
        .collect();
    if !query.is_empty() {
        scored.sort_by_key(|(score, entry)| {
            (
                entry.item.deferred(),
                std::cmp::Reverse(*score),
                entry.item.label().len() - entry.item.name_start(),
                matches!(entry.item, Item::Saved(_)),
            )
        });
    }
    scored.into_iter().map(|(_, entry)| entry).collect()
}

/// The Terminal only command `text` starts with, unless the account lists it.
fn terminal_only_in(text: &str, listed: Option<&Commands>, provider: Provider) -> Option<String> {
    let first = text.split_whitespace().next()?;
    entries(listed, provider, first.strip_prefix('/')?, &[])
        .into_iter()
        .find_map(|e| match e.item {
            Item::Command(command) if command.invocation == first => Some(command),
            _ => None,
        })
        .filter(|command| command.terminal_only)
        .map(|command| command.invocation)
}

fn source_label(item: &Item) -> &'static str {
    let command = match item {
        Item::Saved(saved) if saved.shadowed => return "Saved · command wins",
        Item::Saved(_) => return "Saved",
        Item::Command(command) => command,
    };
    if command.terminal_only {
        return "Terminal only";
    }
    match command.source {
        CommandSource::BuiltIn => "",
        CommandSource::User => "User",
        CommandSource::Project => "Project",
        CommandSource::Plugin => "Plugin",
        CommandSource::Skill => "Skill",
    }
}

impl Keystone {
    /// The selected session's menu rows while the menu is open: the draft has a query,
    /// Escape has not closed it, and no Terminal only notice stands in its place.
    pub(crate) fn slash_entries(&self, cx: &App) -> Option<(u64, Provider, Vec<Entry>)> {
        let session = self.selected_session(cx)?;
        let (id, provider) = (session.id(), session.provider());
        let draft = self.composer.read(cx).value();
        let query = query(draft)?;
        if self.slash.dismissed.as_ref() == Some(&(id, draft.to_owned()))
            || self.slash_notice(cx).is_some()
            || self.palette.is_some()
            || self.prompt_list.is_some()
            || self.import_list.is_some()
            || self.projects.is_some()
        {
            return None;
        }
        let prompts = self.window_prompts(cx);
        let state = self.state.read(cx);
        // A session that cannot ask for commands lists none, rather than loading.
        let listed = match state.catalog_key(id, provider) {
            Some(_) => state.commands(id, provider),
            None => Some(&NONE_LISTED),
        };
        Some((id, provider, entries(listed, provider, query, &prompts)))
    }

    fn slash_notice(&self, cx: &App) -> Option<&Notice> {
        let session = self.selected_session(cx)?;
        let draft = self.composer.read(cx).value();
        self.slash.notice.as_ref().filter(|n| {
            n.session == session.id()
                && n.draft == draft
                && n.provider == session.provider()
                && n.account.as_ref() == session.account()
        })
    }

    /// What the menu says above its rows: why it has none yet, or `None` once listed.
    pub(crate) fn slash_status(
        &self,
        session: u64,
        provider: Provider,
        cx: &App,
    ) -> Option<String> {
        let state = self.state.read(cx);
        if state.catalog_key(session, provider).is_none() {
            return Some(match state.conversation_folder(session) {
                Some(presentation::ConversationFolder::LegacyMissing) => {
                    "Choose a conversation folder to list commands.".to_owned()
                }
                Some(presentation::ConversationFolder::Remote(machine, _)) => format!(
                    "Commands on {} aren't listed yet.",
                    state.machine_label(&machine)
                ),
                Some(
                    presentation::ConversationFolder::Recorded(_)
                    | presentation::ConversationFolder::Unsupported,
                )
                | None => "Commands unavailable for this folder.".to_owned(),
            });
        }
        match state.commands(session, provider)? {
            Commands::Failed(message) => Some(format!("Couldn't list commands. {message}")),
            Commands::Ready(_) => None,
        }
    }

    /// Escape: closes an open menu or notice, and reports whether there was one. A
    /// palette over them takes Escape first.
    pub(crate) fn dismiss_slash(&mut self, cx: &mut Context<Self>) -> bool {
        let open = self.slash_entries(cx).is_some()
            || (self.slash_notice(cx).is_some()
                && self.palette.is_none()
                && self.prompt_list.is_none()
                && self.import_list.is_none()
                && self.projects.is_none());
        if open && let Some(session) = self.selected_session(cx) {
            let draft = self.composer.read(cx).value().to_owned();
            self.slash.dismissed = Some((session.id(), draft));
            self.slash.notice = None;
            cx.notify();
        }
        open
    }

    /// Whether `draft` starts with a Terminal only command, which is then shown with
    /// how to run it instead of being sent.
    pub(crate) fn slash_blocks(&mut self, draft: &ComposerDraft, cx: &mut Context<Self>) -> bool {
        let Some(provider) = self.provider(draft.session, cx) else {
            return false;
        };
        let listed = self.state.read(cx).commands(draft.session, provider);
        let unknown = listed.is_none();
        let Some(invocation) = terminal_only_in(&draft.text, listed, provider) else {
            return false;
        };
        if unknown {
            // The account may list its own command under this name; the answer decides
            // the next Send.
            self.request_catalog(draft.session, provider, cx);
        }
        self.show_terminal_notice(draft.session, provider, invocation, cx);
        true
    }

    fn show_terminal_notice(
        &mut self,
        session: u64,
        provider: Provider,
        invocation: String,
        cx: &mut Context<Self>,
    ) {
        self.slash.notice = Some(Notice {
            session,
            draft: self.composer.read(cx).value().to_owned(),
            invocation,
            provider,
            account: self.session(session, cx).and_then(|s| s.account()).cloned(),
            copied: false,
        });
        cx.notify();
    }

    /// The command that opens the provider's terminal in the session's folder on its
    /// account, as they are now. A session on another machine has no command here: its
    /// terminal would be on that machine, so this says so instead, as `Err`.
    fn notice_command(&self, notice: &Notice, cx: &App) -> Result<String, String> {
        let state = self.state.read(cx);
        let home = notice.account.as_ref().and_then(|id| {
            state
                .store()
                .workspace()
                .accounts()
                .iter()
                .find(|a| a.id() == id)
                .and_then(|a| a.home().map(std::path::Path::to_path_buf))
        });
        let folder = match state.place(notice.session) {
            Some(presentation::SessionPlace::Local(directory)) => directory.path().to_path_buf(),
            Some(presentation::SessionPlace::Remote(machine)) => {
                return Err(format!(
                    "{} runs only in a terminal, and Keystone can't open one on {machine} yet.",
                    notice.invocation
                ));
            }
            Some(presentation::SessionPlace::Unusable) | None => self.presentation_folder(),
        };
        Ok(terminal_command(notice.provider, home.as_deref(), &folder))
    }

    /// The project folder, for a session whose own folder this version cannot use.
    fn presentation_folder(&self) -> std::path::PathBuf {
        match &self.presentation.project {
            Ok(project) => project.path().to_path_buf(),
            Err(_) => std::path::PathBuf::from("."),
        }
    }

    /// Chooses the highlighted row, or `row`.
    fn choose_slash(&mut self, row: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some((session, provider, entries)) = self.slash_entries(cx) else {
            return;
        };
        let typed = query(self.composer.read(cx).value()).unwrap_or_default();
        if row.is_none()
            && entries.iter().any(|entry| {
                matches!(&entry.item, Item::Saved(saved)
                    if saved.pending && saved.label[1..].eq_ignore_ascii_case(typed))
            })
        {
            return;
        }
        let Some(entry) = row
            .or_else(|| self.slash.list.read(cx).highlighted())
            .and_then(|row| entries.into_iter().nth(row))
        else {
            return;
        };
        let command = match entry.item {
            Item::Saved(saved) if saved.pending && row.is_none() => return,
            Item::Saved(saved) => return self.insert_prompt(saved.id, true, window, cx),
            Item::Command(command) if command.terminal_only => {
                return self.show_terminal_notice(session, provider, command.invocation, cx);
            }
            Item::Command(command) => command,
        };
        self.composer.update(cx, |input, cx| {
            input.set_value(format!("{} ", command.invocation), cx)
        });
        self.save_draft(cx);
        self.composer.focus_handle(cx).focus(window, cx);
        self.refresh(cx);
    }

    /// What the draft's leading command takes, for the composer's hint.
    pub(crate) fn slash_hint(&self, session: u64, provider: Provider, cx: &App) -> Option<String> {
        let draft = self.composer.read(cx).value();
        let (first, _) = draft.split_once(char::is_whitespace)?;
        let listed = self.state.read(cx).commands(session, provider);
        let Some(Commands::Ready(commands)) = listed else {
            return None;
        };
        let command = commands.iter().find(|c| c.invocation == first)?;
        (!command.argument_hint.is_empty()).then(|| format!("{first} {}", command.argument_hint))
    }

    /// Lets Up, Down, Enter and Tab drive the open menu while the composer keeps focus.
    pub(crate) fn slash_keys(
        &self,
        field: gpui::Stateful<gpui::Div>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        self.menu_keys(field, self.slash.list.clone(), Self::choose_slash, cx)
    }

    /// Lets Up and Down move `list`'s highlight and Enter (without Shift) and Tab call
    /// `choose` with no row, for a menu above the composer that leaves it focus.
    pub(crate) fn menu_keys(
        &self,
        field: gpui::Stateful<gpui::Div>,
        list: Entity<VirtualListState>,
        choose: fn(&mut Self, Option<usize>, &mut Window, &mut Context<Self>),
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let host = cx.entity_id();
        let step = move |delta: isize| {
            let list = list.clone();
            move |cx: &mut App| {
                list.update(cx, |list, _| list.move_by(delta));
                cx.notify(host);
            }
        };
        let (up, down) = (step(-1), step(1));
        field
            .capture_action(move |_: &text_input::Up, _, cx| {
                up(cx);
                cx.stop_propagation();
            })
            .capture_action(move |_: &text_input::Down, _, cx| {
                down(cx);
                cx.stop_propagation();
            })
            .capture_action(self.scoped_listener(
                cx,
                move |this, _: &text_input::Newline, window, cx| {
                    if window.modifiers().shift {
                        return;
                    }
                    cx.stop_propagation();
                    choose(this, None, window, cx);
                },
            ))
            .capture_action(
                self.scoped_listener(cx, move |this, _: &NextFocus, window, cx| {
                    cx.stop_propagation();
                    choose(this, None, window, cx);
                }),
            )
    }

    /// The menu, or the Terminal only notice, above the composer.
    pub(crate) fn slash_menu(
        &self,
        window_width: Pixels,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let card = if let Some(notice) = self.slash_notice(cx) {
            self.terminal_notice(notice, cx)
        } else {
            let (session, provider, entries) = self.slash_entries(cx)?;
            let state = self.state.read(cx);
            let loading = state.catalog_key(session, provider).is_some()
                && state.commands(session, provider).is_none();
            let failed = matches!(state.commands(session, provider), Some(Commands::Failed(_)));
            let status = if loading {
                // Asks once; a request already on its way is not repeated.
                let view = cx.weak_entity();
                cx.defer(move |cx| {
                    let _ = view.update(cx, |this, cx| this.request_catalog(session, provider, cx));
                });
                Some("Loading commands…".to_owned())
            } else {
                self.slash_status(session, provider, cx)
            };
            let retry = failed.then(|| {
                self.scoped_listener(cx, move |this, _: &gpui::ClickEvent, _, cx| {
                    this.retry_catalog(session, provider, cx)
                })
            });
            self.command_list(entries, status, retry, cx)
        };
        Some(composer_overlay(card, window_width))
    }

    fn command_list(
        &self,
        entries: Vec<Entry>,
        status: Option<String>,
        retry: Option<impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let count = entries.len();
        let row_height = cx.theme().density.control(px(32.));
        let entries = std::rc::Rc::new(entries);
        let rows = move |row: usize, highlighted: bool, _: &mut Window, cx: &mut App| {
            entries.get(row).map_or_else(
                || div().h(row_height).into_any_element(),
                |entry| command_row(entry, highlighted, row_height, cx),
            )
        };
        let choose = self.scoped_listener(cx, |this, row: &usize, window, cx| {
            this.choose_slash(Some(*row), window, cx)
        });
        let list = self.slash.list.clone();
        self.menu_card(
            "slash-menu",
            &list,
            count,
            rows,
            choose,
            status,
            retry,
            Some("No matching commands"),
            cx,
        )
    }

    /// A menu above the composer: a status line with an optional Retry, then `count`
    /// rows from `rows` in `list`, or any `empty` label when there are none. Its parts are named
    /// `{id}`, `{id}-status`, `{id}-retry` and `{id}-list-row-{n}`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn menu_card(
        &self,
        id: &'static str,
        list: &Entity<VirtualListState>,
        count: usize,
        rows: impl Fn(usize, bool, &mut Window, &mut App) -> AnyElement + 'static,
        choose: impl Fn(&usize, &mut Window, &mut App) + 'static,
        status: Option<String>,
        retry: Option<impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static>,
        empty: Option<&'static str>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        list.update(cx, |list, _| {
            if list.count() != count {
                list.set_count(count);
            }
        });
        let row_height = cx.theme().density.control(px(32.));
        v_flex()
            .id(id)
            .automation_id(id)
            .occlude()
            .p(px(4.))
            .surface(Elevation::Overlay, cx)
            .children(status.map(|status| {
                h_flex()
                    .id(format!("{id}-status"))
                    .automation_id(format!("{id}-status"))
                    .gap(spacing::SM)
                    .px(spacing::SM)
                    .py(px(6.))
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(status)
                                .size(TextSize::Sm)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                    )
                    .children(retry.map(|retry| {
                        Button::new(format!("{id}-retry"), "Retry")
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Compact)
                            .debug_selector(move || format!("{id}-retry"))
                            .on_click(retry)
                    }))
            }))
            .when_some(empty.filter(|_| count == 0), |d, empty| {
                d.child(
                    div()
                        .px(spacing::SM)
                        .py(px(6.))
                        .child(Label::new(empty).color(Color::Muted)),
                )
            })
            .when(count > 0, |d| {
                d.child(
                    VirtualList::new(format!("{id}-list"), list, rows)
                        .focusable(false)
                        .on_confirm(choose)
                        .h(row_height * count.min(VISIBLE_ROWS) as f32),
                )
            })
    }

    fn terminal_notice(
        &self,
        notice: &Notice,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let command = match self.notice_command(notice, cx) {
            Ok(command) => command,
            Err(elsewhere) => {
                return v_flex()
                    .id("slash-terminal")
                    .automation_id("slash-terminal-elsewhere")
                    .occlude()
                    .p(spacing::MD)
                    .surface(Elevation::Overlay, cx)
                    .child(Label::new(elsewhere).size(TextSize::Ui).color(Color::Muted));
            }
        };
        let copied = notice.copied;
        v_flex()
            .id("slash-terminal")
            .automation_id("slash-terminal")
            .occlude()
            .p(spacing::MD)
            .gap(spacing::SM)
            .surface(Elevation::Overlay, cx)
            .child(
                Label::new(format!(
                    "{} runs only in a terminal. Run this, then type {}:",
                    notice.invocation, notice.invocation
                ))
                .size(TextSize::Ui),
            )
            .child(
                h_flex()
                    .gap(spacing::SM)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .px(spacing::SM)
                            .py(px(6.))
                            .rounded(radius::CONTROL)
                            .bg(cx.theme().colors.element_filled)
                            .child(
                                Label::new(command.clone())
                                    .size(TextSize::Sm)
                                    .mono()
                                    .truncate(),
                            ),
                    )
                    .child(
                        IconButton::new(
                            "slash-terminal-copy",
                            if copied {
                                IconName::Check
                            } else {
                                IconName::Copy
                            },
                        )
                        .style(ButtonStyle::Subtle)
                        .size(ButtonSize::Compact)
                        .debug_selector(|| "slash-terminal-copy".into())
                        .aria_label("Copy terminal command")
                        .tooltip(Tooltip::text(if copied {
                            "Copied"
                        } else {
                            "Copy terminal command"
                        }))
                        .on_click(self.scoped_listener(
                            cx,
                            move |this, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(command.clone()));
                                if let Some(notice) = &mut this.slash.notice {
                                    notice.copied = true;
                                }
                                cx.notify();
                            },
                        )),
                    ),
            )
    }
}

/// Pins `card` above the composer, as wide as the conversation column allows.
pub(crate) fn composer_overlay(
    card: gpui::Stateful<gpui::Div>,
    window_width: Pixels,
) -> AnyElement {
    let width = px(CONTENT_WIDTH).min(window_width - px(32.));
    div()
        .absolute()
        .size_0()
        .top_0()
        .left_0()
        .child(
            deferred(
                anchored()
                    .anchor(Anchor::BottomLeft)
                    .snap_to_window_with_margin(px(8.))
                    .child(div().pb(px(6.)).child(card.w(width))),
            )
            .priority(1),
        )
        .into_any_element()
}

fn command_row(entry: &Entry, highlighted: bool, height: Pixels, cx: &mut App) -> AnyElement {
    let item = &entry.item;
    let label = item.label();
    let marked = HighlightStyle {
        color: Some(Color::Strong.to_hsla(cx)),
        font_weight: Some(FontWeight::BOLD),
        ..Default::default()
    };
    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    for &start in &entry.positions {
        let end = start + label[start..].chars().next().map_or(0, char::len_utf8);
        match ranges.last_mut() {
            Some(last) if last.end == start => last.end = end,
            _ => ranges.push(start..end),
        }
    }
    let name = StyledText::new(label.to_owned())
        .with_highlights(ranges.into_iter().map(|range| (range, marked)));
    h_flex()
        .h(height)
        .px(spacing::SM)
        .gap(spacing::SM)
        .rounded(radius::MENU_ROW)
        .when(highlighted, |d| d.bg(cx.theme().colors.selection_fill))
        .child(div().flex_none().text_size(TextSize::Ui.px()).child(name))
        .when(!item.argument_hint().is_empty(), |d| {
            d.child(
                div().flex_shrink(1.).max_w(px(180.)).min_w_0().child(
                    Label::new(item.argument_hint().to_owned())
                        .size(TextSize::Sm)
                        .color(Color::Faint)
                        .truncate(),
                ),
            )
        })
        .child(
            div().flex_1().min_w_0().child(
                Label::new(item.description().to_owned())
                    .size(TextSize::Sm)
                    .color(Color::Muted)
                    .truncate(),
            ),
        )
        .child(
            div()
                .flex_none()
                .child(Label::new(source_label(item)).size(TextSize::Sm).color(
                    if item.terminal_only() || matches!(item, Item::Saved(s) if s.shadowed) {
                        Color::Muted
                    } else {
                        Color::Faint
                    },
                )),
        )
        .into_any_element()
}

#[cfg(all(test, feature = "ui-tests"))]
#[path = "slash_menu_tests.rs"]
mod ui_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn command(invocation: &str, source: CommandSource) -> SlashCommand {
        SlashCommand {
            invocation: invocation.into(),
            description: String::new(),
            argument_hint: String::new(),
            source,
            terminal_only: false,
        }
    }

    fn invocations(entries: Vec<Entry>) -> Vec<String> {
        entries
            .into_iter()
            .map(|e| e.item.label().to_owned())
            .collect()
    }

    #[test]
    fn the_query_is_the_first_word_after_a_leading_slash() {
        assert_eq!(query("/"), Some(""));
        assert_eq!(query("/fix"), Some("fix"));
        assert_eq!(query("/fix "), None);
        assert_eq!(query("fix"), None);
        assert_eq!(query(" /fix"), None);
    }

    #[test]
    fn listed_commands_come_first_and_terminal_only_ones_the_account_lists_are_sendable() {
        let listed = Commands::Ready(vec![
            command("/fixture-review", CommandSource::User),
            command("/status", CommandSource::BuiltIn),
        ]);
        let all = invocations(entries(Some(&listed), Provider::Claude, "", &[]));
        assert_eq!(&all[..3], ["/fixture-review", "/status", "/login"]);
        assert_eq!(all.iter().filter(|i| *i == "/status").count(), 1);
        assert_eq!(
            invocations(entries(Some(&listed), Provider::Claude, "rev", &[])),
            ["/fixture-review"]
        );
        // Before the account answers, the Terminal only built-ins still show.
        assert_eq!(
            invocations(entries(None, Provider::Claude, "logi", &[])),
            ["/login"]
        );
        assert_eq!(
            terminal_only_in("/login now", None, Provider::Claude),
            Some("/login".into())
        );
        assert_eq!(
            terminal_only_in("/status", Some(&listed), Provider::Claude),
            None
        );
        assert_eq!(
            terminal_only_in("please /login", None, Provider::Claude),
            None
        );
        assert_eq!(terminal_only_in("/loginx", None, Provider::Claude), None);
    }

    fn prompts(shortcuts: &[&str]) -> keystone_core::Store {
        let mut store = keystone_core::Store::in_memory().unwrap();
        for shortcut in shortcuts {
            store
                .add_prompt(keystone_core::PromptFields {
                    title: format!("Title {shortcut}"),
                    body: "body".into(),
                    shortcut: (*shortcut).into(),
                    project: None,
                })
                .unwrap();
        }
        store
    }

    /// A shortcut a command shares in another case still loses to it, and while the
    /// account's list loads a prompt with a shortcut waits behind every command, since
    /// one may take its name.
    #[test]
    fn a_command_wins_in_any_case_and_a_shortcut_waits_while_commands_load() {
        let store = prompts(&["Fixture-Review", "", "log"]);
        let saved: Vec<&Prompt> = store.workspace().prompts().iter().collect();
        let listed = Commands::Ready(vec![command("/fixture-review", CommandSource::User)]);
        let all = entries(Some(&listed), Provider::Claude, "", &saved);
        let labels = invocations(entries(Some(&listed), Provider::Claude, "", &saved));
        assert_eq!(labels[..3], ["Title", "/log", "/fixture-review"]);
        assert_eq!(labels.last().unwrap(), "/Fixture-Review");
        assert_eq!(
            source_label(&all.last().unwrap().item),
            "Saved · command wins"
        );
        let typed = invocations(entries(
            Some(&listed),
            Provider::Claude,
            "fixture-review",
            &saved,
        ));
        assert_eq!(typed[..2], ["/fixture-review", "/Fixture-Review"]);

        // Before the list: `/log` would beat `/login` on its own, but waits.
        let loading = invocations(entries(None, Provider::Claude, "log", &saved));
        assert_eq!(loading.first().map(String::as_str), Some("/login"));
        assert_eq!(loading.last().map(String::as_str), Some("/log"));
        let ready = invocations(entries(Some(&listed), Provider::Claude, "log", &saved));
        assert_eq!(ready.first().map(String::as_str), Some("/log"));
    }

    /// A prompt without a shortcut whose title is a command's name matches as well
    /// and is as long, so the command comes first and Enter chooses it.
    #[test]
    fn a_command_comes_before_a_prompt_titled_with_its_name() {
        let mut store = keystone_core::Store::in_memory().unwrap();
        for title in ["review", "Review"] {
            store
                .add_prompt(keystone_core::PromptFields {
                    title: title.into(),
                    body: "body".into(),
                    shortcut: String::new(),
                    project: None,
                })
                .unwrap();
        }
        let saved: Vec<&Prompt> = store.workspace().prompts().iter().collect();
        let listed = Commands::Ready(vec![command("/review", CommandSource::User)]);
        let typed = invocations(entries(Some(&listed), Provider::Claude, "review", &saved));
        assert_eq!(typed, ["/review", "review", "Review"]);
    }

    #[test]
    fn a_codex_skill_matches_by_name_and_keeps_its_dollar() {
        let listed = Commands::Ready(vec![command("$review", CommandSource::Skill)]);
        let found = entries(Some(&listed), Provider::Codex, "review", &[]);
        assert_eq!(found[0].item.label(), "$review");
        assert_eq!(found[0].positions, [1, 2, 3, 4, 5, 6]);
        assert!(
            found
                .iter()
                .any(|e| e.item.label() == "/review" && e.item.terminal_only())
        );
    }
}
