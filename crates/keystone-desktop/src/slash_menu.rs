//! The composer's `/` menu. A draft that starts with `/` and has no space yet lists the
//! commands and skills the session's account offers in its folder, from the same
//! request as the model catalog, so it works before the first Send. Each row names its
//! source. Choosing a row replaces the draft with its invocation and a space, and the
//! composer's hint shows what it takes. Built-ins that only the provider's own terminal
//! runs are marked Terminal only: choosing or sending one sends nothing and shows the
//! command that opens that terminal instead.
//!
//! Other sources (saved prompts) can join through [`entries`].

use crate::{CONTENT_WIDTH, ComposerDraft, Keystone, NextFocus, presentation};
use gpui::{
    Anchor, AnyElement, App, ClipboardItem, Context, Entity, Focusable, FontWeight, HighlightStyle,
    IntoElement, Pixels, StyledText, Subscription, Window, anchored, deferred, div, prelude::*, px,
};
use keystone_agent::{CommandSource, Commands, SlashCommand, terminal_command, terminal_only};
use keystone_core::{AccountId, Provider};
use keystone_ui::{fuzzy, prelude::*, text_input};

const VISIBLE_ROWS: usize = 8;

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

/// One row: a command and the bytes of its invocation the query matched.
pub(crate) struct Entry {
    pub command: SlashCommand,
    positions: Vec<usize>,
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

/// The rows for `query`: what the account lists, then the provider's Terminal only
/// built-ins it does not list. An empty query keeps that order; otherwise the best
/// match comes first, then the shorter name.
pub(crate) fn entries(listed: Option<&Commands>, provider: Provider, query: &str) -> Vec<Entry> {
    let mut commands: Vec<SlashCommand> = match listed {
        Some(Commands::Ready(commands)) => commands.clone(),
        _ => Vec::new(),
    };
    let terminal: Vec<_> = terminal_only(provider)
        .into_iter()
        .filter(|t| !commands.iter().any(|c| c.invocation == t.invocation))
        .collect();
    commands.extend(terminal);
    let mut scored: Vec<(i32, Entry)> = commands
        .into_iter()
        .filter_map(|command| {
            let found = fuzzy::score(query, command.name())?;
            // The invocation's first byte is its `/` or `$`.
            let positions = found.positions.iter().map(|p| p + 1).collect();
            Some((found.score, Entry { command, positions }))
        })
        .collect();
    if !query.is_empty() {
        scored.sort_by_key(|(score, entry)| {
            (std::cmp::Reverse(*score), entry.command.invocation.len())
        });
    }
    scored.into_iter().map(|(_, entry)| entry).collect()
}

/// The Terminal only command `text` starts with, unless the account lists it.
fn terminal_only_in(text: &str, listed: Option<&Commands>, provider: Provider) -> Option<String> {
    let first = text.split_whitespace().next()?;
    entries(listed, provider, first.strip_prefix('/')?)
        .into_iter()
        .find(|e| e.command.invocation == first)
        .filter(|e| e.command.terminal_only)
        .map(|e| e.command.invocation)
}

fn source_label(command: &SlashCommand) -> &'static str {
    if command.terminal_only {
        return "Terminal only";
    }
    match command.source {
        CommandSource::BuiltIn => "Built-in",
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
        {
            return None;
        }
        let listed = self.state.read(cx).commands(id, provider);
        Some((id, provider, entries(listed, provider, query)))
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
            return Some(
                match state.conversation_folder(session) {
                    Some(presentation::ConversationFolder::LegacyMissing) => {
                        "Choose a conversation folder to list commands."
                    }
                    _ => "Commands need this conversation's saved folder, which this version cannot use.",
                }
                .to_owned(),
            );
        }
        match state.commands(session, provider)? {
            Commands::Failed(message) => Some(format!("Couldn't list commands. {message}")),
            Commands::Ready(_) => None,
        }
    }

    /// Escape: closes an open menu or notice, and reports whether there was one.
    pub(crate) fn dismiss_slash(&mut self, cx: &mut Context<Self>) -> bool {
        let open = self.slash_entries(cx).is_some() || self.slash_notice(cx).is_some();
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
    /// account, as they are now.
    fn notice_command(&self, notice: &Notice, cx: &App) -> String {
        let state = self.state.read(cx);
        let home = notice.account.as_ref().and_then(|id| {
            state
                .store()
                .workspace()
                .accounts()
                .iter()
                .find(|a| a.id() == id)
                .map(|a| a.home().to_path_buf())
        });
        let folder = match state.conversation_folder(notice.session) {
            Some(presentation::ConversationFolder::Recorded(directory)) => {
                directory.path().to_path_buf()
            }
            _ => self.presentation_folder(),
        };
        terminal_command(notice.provider, home.as_deref(), &folder)
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
        let Some(entry) = row
            .or_else(|| self.slash.list.read(cx).highlighted())
            .and_then(|row| entries.into_iter().nth(row))
        else {
            return;
        };
        if entry.command.terminal_only {
            self.show_terminal_notice(session, provider, entry.command.invocation, cx);
            return;
        }
        self.composer.update(cx, |input, cx| {
            input.set_value(format!("{} ", entry.command.invocation), cx)
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
        let program = match notice.provider {
            Provider::Claude => "Claude Code",
            Provider::Codex => "Codex",
        };
        let command = self.notice_command(notice, cx);
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
                    "{} runs only in {program}'s own terminal, so Keystone doesn't send it. Open it in a terminal, then type {}:",
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
                            .child(Label::new(command.clone()).size(TextSize::Sm).mono().truncate()),
                    )
                    .child(
                        IconButton::new("slash-terminal-copy", if copied { IconName::Check } else { IconName::Copy })
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Compact)
                            .debug_selector(|| "slash-terminal-copy".into())
                            .aria_label("Copy terminal command")
                            .tooltip(Tooltip::text(if copied { "Copied" } else { "Copy terminal command" }))
                            .on_click(self.scoped_listener(cx, move |this, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(command.clone()));
                                if let Some(notice) = &mut this.slash.notice {
                                    notice.copied = true;
                                }
                                cx.notify();
                            })),
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
    let command = &entry.command;
    let marked = HighlightStyle {
        color: Some(Color::Accent.to_hsla(cx)),
        font_weight: Some(FontWeight::BOLD),
        ..Default::default()
    };
    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    for &start in &entry.positions {
        let end = start
            + command.invocation[start..]
                .chars()
                .next()
                .map_or(0, char::len_utf8);
        match ranges.last_mut() {
            Some(last) if last.end == start => last.end = end,
            _ => ranges.push(start..end),
        }
    }
    let name = StyledText::new(command.invocation.clone())
        .with_highlights(ranges.into_iter().map(|range| (range, marked)));
    h_flex()
        .h(height)
        .px(spacing::SM)
        .gap(spacing::SM)
        .rounded(radius::MENU_ROW)
        .when(highlighted, |d| d.bg(cx.theme().colors.selection_fill))
        .child(div().flex_none().text_size(TextSize::Ui.px()).child(name))
        .when(!command.argument_hint.is_empty(), |d| {
            d.child(
                div().flex_shrink(1.).max_w(px(180.)).min_w_0().child(
                    Label::new(command.argument_hint.clone())
                        .size(TextSize::Sm)
                        .color(Color::Faint)
                        .truncate(),
                ),
            )
        })
        .child(
            div().flex_1().min_w_0().child(
                Label::new(
                    command
                        .description
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .to_owned(),
                )
                .size(TextSize::Sm)
                .color(Color::Muted)
                .truncate(),
            ),
        )
        .child(
            div()
                .flex_none()
                .child(Label::new(source_label(command)).size(TextSize::Sm).color(
                    if command.terminal_only {
                        Color::Warning
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
        entries.into_iter().map(|e| e.command.invocation).collect()
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
        let all = invocations(entries(Some(&listed), Provider::Claude, ""));
        assert_eq!(&all[..3], ["/fixture-review", "/status", "/login"]);
        assert_eq!(all.iter().filter(|i| *i == "/status").count(), 1);
        assert_eq!(
            invocations(entries(Some(&listed), Provider::Claude, "rev")),
            ["/fixture-review"]
        );
        // Before the account answers, the Terminal only built-ins still show.
        assert_eq!(
            invocations(entries(None, Provider::Claude, "logi")),
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

    #[test]
    fn a_codex_skill_matches_by_name_and_keeps_its_dollar() {
        let listed = Commands::Ready(vec![command("$review", CommandSource::Skill)]);
        let found = entries(Some(&listed), Provider::Codex, "review");
        assert_eq!(found[0].command.invocation, "$review");
        assert_eq!(found[0].positions, [1, 2, 3, 4, 5, 6]);
        assert!(
            found
                .iter()
                .any(|e| e.command.invocation == "/review" && e.command.terminal_only)
        );
    }
}
