//! Import sessions…: a conversation Claude Code or Codex already keeps for the
//! window's project folder, brought in as a Keystone session that continues it. The
//! user picks an account, then a conversation; the new session shows its newest
//! turns, and its next message resumes the provider's conversation. Nothing is
//! written to the provider's store.

use crate::app_state::Native;
use crate::model_selector::{account_label, provider_name};
use crate::relative_time::{LocalTime, relative};
use crate::{Keystone, now_ms};
use gpui::{AppContext, Context, Entity, FocusHandle, Focusable, IntoElement, Window};
use keystone_agent::import::Listed;
use keystone_core::{AccountId, Provider, WorkingDirectory};
use keystone_ui::{CommandPalette, CommandPaletteState, PaletteItem};

gpui::actions!(keystone, [ImportSessions]);

const ACCOUNT: &str = "account:";
const SESSION: &str = "session:";

/// The open import list and the focus it returns to when it closes.
pub(crate) struct ImportList {
    pub(crate) state: Entity<CommandPaletteState>,
    restore: Option<FocusHandle>,
    step: Step,
}

enum Step {
    Accounts(Vec<(Provider, Option<AccountId>)>),
    Sessions {
        provider: Provider,
        account: Option<AccountId>,
        /// The project folder listed, which the imported session works in.
        directory: WorkingDirectory,
        /// The listing request still out, if any.
        waiting: Option<u64>,
        listed: Vec<Listed>,
        /// What the card says when it lists nothing.
        empty: String,
        /// The conversation being read for its import, and the request reading it.
        reading: Option<(u64, Listed)>,
    },
}

/// The most characters of a title a row shows.
const LABEL_CHARS: usize = 72;

/// A conversation's title on one line, cut to [`LABEL_CHARS`], or its ID without one.
fn label(session: &Listed) -> String {
    let line = session
        .title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    match line.char_indices().nth(LABEL_CHARS) {
        _ if line.is_empty() => session.id.clone(),
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line,
    }
}

/// `n` turns, with a plus when only part of the history was counted.
fn turns(n: usize, more: bool) -> String {
    let plus = if more { "+" } else { "" };
    match (n, more) {
        (1, false) => "1 turn".into(),
        _ => format!("{n}{plus} turns"),
    }
}

impl Keystone {
    /// Opens the import list on its accounts, or closes it.
    pub(crate) fn open_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.import_list.is_some() {
            return self.close_import(window, cx);
        }
        self.new_menu = false;
        let restore = self
            .palette
            .take()
            .and_then(|palette| palette.restore)
            .or_else(|| window.focused(cx));
        let workspace = self.state.read(cx).store().workspace();
        let accounts: Vec<(Provider, Option<AccountId>)> = [Provider::Claude, Provider::Codex]
            .into_iter()
            .flat_map(|provider| {
                std::iter::once((provider, None)).chain(
                    workspace
                        .accounts()
                        .iter()
                        .filter(move |a| a.provider() == provider)
                        .map(move |a| (provider, Some(a.id().clone()))),
                )
            })
            .collect();
        let items = accounts
            .iter()
            .enumerate()
            .map(|(n, (provider, account))| {
                let label =
                    match account_label(*provider, account.as_ref(), workspace.accounts(), None) {
                        Some(account) => format!("{} · {account}", provider_name(*provider)),
                        None => provider_name(*provider).to_owned(),
                    };
                PaletteItem::new(format!("{ACCOUNT}{n}"), label)
            })
            .collect();
        let state = cx.new(|cx| CommandPaletteState::with_placeholder(items, "Import from", cx));
        state.focus_handle(cx).focus(window, cx);
        self.import_list = Some(ImportList {
            state,
            restore,
            step: Step::Accounts(accounts),
        });
        self.refresh(cx);
    }

    fn close_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(restore) = self.drop_import(cx) else {
            return;
        };
        restore
            .unwrap_or_else(|| self.focus.clone())
            .focus(window, cx);
        self.refresh(cx);
    }

    /// Closes the import list without moving focus, dropping the answers it waits on,
    /// and returns the focus it would have restored.
    pub(crate) fn drop_import(&mut self, cx: &mut Context<Self>) -> Option<Option<FocusHandle>> {
        let list = self.import_list.take()?;
        if let Step::Sessions {
            waiting, reading, ..
        } = list.step
        {
            let tickets = waiting.into_iter().chain(reading.map(|(ticket, _)| ticket));
            self.state
                .update(cx, |state, _| tickets.for_each(|t| state.abandon_native(t)));
        }
        Some(list.restore)
    }

    fn confirm_import(&mut self, item: &PaletteItem, window: &mut Window, cx: &mut Context<Self>) {
        let Some(list) = &self.import_list else {
            return;
        };
        match &list.step {
            Step::Accounts(accounts) => {
                let Some((provider, account)) = item
                    .id
                    .strip_prefix(ACCOUNT)
                    .and_then(|n| n.parse::<usize>().ok())
                    .and_then(|n| accounts.get(n).cloned())
                else {
                    return;
                };
                self.list_sessions(provider, account, &item.label, window, cx);
                self.poll_import(window, cx);
            }
            Step::Sessions {
                provider,
                account,
                directory,
                listed,
                reading: None,
                ..
            } => {
                let Some(chosen) = item
                    .id
                    .strip_prefix(SESSION)
                    .and_then(|id| listed.iter().find(|l| l.id == id).cloned())
                else {
                    return;
                };
                let (provider, account) = (*provider, account.clone());
                if let Some(session) = self.imported(provider, &chosen.id, cx) {
                    self.close_import(window, cx);
                    return self.select(session, cx);
                }
                let directory = directory.path().to_owned();
                let thread = chosen.id.clone();
                let ticket = self.state.update(cx, |state, _| {
                    state.read_native(provider, account.as_ref(), &directory, thread)
                });
                if let Some(ImportList {
                    step: Step::Sessions { reading, .. },
                    ..
                }) = &mut self.import_list
                {
                    *reading = Some((ticket, chosen));
                }
                self.poll_import(window, cx);
            }
            Step::Sessions { .. } => {}
        }
    }

    /// The session already imported from `provider`'s conversation `thread`, if any; a
    /// second import opens it instead.
    fn imported(&self, provider: Provider, thread: &str, cx: &Context<Self>) -> Option<u64> {
        let workspace = self.state.read(cx).store().workspace();
        workspace.imported(provider, thread).map(|s| s.id())
    }

    /// Lists `account`'s conversations for this window's project folder, under its
    /// `name`.
    fn list_sessions(
        &mut self,
        provider: Provider,
        account: Option<AccountId>,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let directory = match self.presentation.creation_directory() {
            Ok(directory) => directory,
            Err(error) => {
                self.error = Some(error);
                return self.close_import(window, cx);
            }
        };
        let waiting = self.state.update(cx, |state, _| {
            state.list_native(provider, account.as_ref(), directory.path())
        });
        let Some(list) = &mut self.import_list else {
            return;
        };
        list.step = Step::Sessions {
            provider,
            account,
            directory,
            waiting: Some(waiting),
            listed: Vec::new(),
            empty: "Loading…".into(),
            reading: None,
        };
        list.state = cx.new(|cx| CommandPaletteState::with_placeholder(Vec::new(), name, cx));
        list.state.focus_handle(cx).focus(window, cx);
        self.refresh(cx);
    }

    /// Takes the answer the open import list waits for, once it has arrived: the
    /// listing to show, or the conversation to import.
    pub(crate) fn poll_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ImportList {
            step:
                Step::Sessions {
                    provider,
                    account,
                    directory,
                    waiting,
                    listed,
                    empty,
                    reading,
                },
            state: palette,
            ..
        }) = &mut self.import_list
        else {
            return;
        };
        if let Some(ticket) = *waiting
            && let Some(Native::Listed(result)) =
                self.state.update(cx, |state, _| state.take_native(ticket))
        {
            *waiting = None;
            match result {
                Ok(found) => {
                    *listed = found;
                    *empty = "No sessions in this folder.".into();
                }
                Err(error) => *empty = error,
            }
            let workspace = self.state.read(cx).store().workspace();
            let now = LocalTime::at(now_ms());
            let items = listed
                .iter()
                .map(|session| {
                    let mut detail: Vec<String> = session
                        .updated
                        .map(|ms| relative(now, LocalTime::at(ms)))
                        .into_iter()
                        .collect();
                    detail.extend(session.turns.map(|n| turns(n, session.more)));
                    if workspace.imported(*provider, &session.id).is_some() {
                        detail.push("Imported".into());
                    }
                    PaletteItem::new(format!("{SESSION}{}", session.id), label(session))
                        .detail(detail.join(" · "))
                })
                .collect();
            palette.update(cx, |state, cx| state.set_items(items, cx));
            self.refresh(cx);
            return;
        }
        let Some((ticket, chosen)) = reading.clone() else {
            return;
        };
        let Some(Native::Read(result)) =
            self.state.update(cx, |state, _| state.take_native(ticket))
        else {
            return;
        };
        let (provider, account, directory) = (*provider, account.clone(), directory.clone());
        self.close_import(window, cx);
        let snapshot = match result {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.error = Some(format!("Not imported. {error}"));
                return self.refresh(cx);
            }
        };
        if let Some(session) = self.imported(provider, &chosen.id, cx) {
            return self.select(session, cx);
        }
        let window_id = self.window_id;
        self.create_with(cx, |state, cx| {
            state.import(
                provider,
                account,
                &directory,
                chosen.id,
                &chosen.title,
                &snapshot,
                now_ms(),
                window_id,
                cx,
            )
        });
        if self.error.is_none() {
            self.composer.focus_handle(cx).focus(window, cx);
        }
    }

    pub(crate) fn import_overlay(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let list = self.import_list.as_ref()?;
        let empty = match &list.step {
            Step::Accounts(_) => "No matches".to_owned(),
            Step::Sessions { empty, .. } => empty.clone(),
        };
        let dismiss =
            self.scoped_listener(cx, |this, _: &(), window, cx| this.close_import(window, cx));
        Some(
            CommandPalette::new("import", &list.state)
                .empty_label(empty)
                .on_confirm(
                    self.scoped_listener(cx, |this, item: &PaletteItem, window, cx| {
                        this.confirm_import(item, window, cx)
                    }),
                )
                .on_dismiss(move |window, cx| dismiss(&(), window, cx)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_is_one_short_line_and_an_untitled_one_shows_its_id() {
        let listed = |title: &str| Listed {
            id: "t1".into(),
            title: title.into(),
            updated: None,
            turns: Some(1),
            more: false,
        };
        assert_eq!(label(&listed("Fix  the\nbuild")), "Fix the build");
        assert_eq!(label(&listed(" ")), "t1");
        let long = label(&listed(&"é".repeat(100)));
        assert_eq!(long.chars().count(), LABEL_CHARS + 1);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn turn_counts_read_as_words() {
        assert_eq!(turns(1, false), "1 turn");
        assert_eq!(turns(12, false), "12 turns");
        assert_eq!(turns(1000, true), "1000+ turns");
    }
}
