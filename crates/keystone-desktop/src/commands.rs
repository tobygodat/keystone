//! The commands the palette lists. Their key bindings come from the same table, so a
//! row's shortcut hint cannot drift from the binding that runs it.

use crate::attachments::AttachFiles;
use crate::changes::{CopyPullRequestLink, ToggleChanges};
use crate::customization::OpenCustomize;
use crate::files::ToggleFiles;
use crate::fork::ForkFromHere;
use crate::handoff::HandOff;
use crate::notes::ToggleNotes;
use crate::projects::{OpenProject, SwitchProject};
use crate::prompts::InsertPrompt;
use crate::quick::OpenQuickComposer;
use crate::second_opinion::AskSecondOpinion;
use crate::session_tabs::{CloseTab, NewTab, NextTab, PreviousTab, SplitRight};
use crate::transcript_keyboard::*;
use crate::windows::{self, CloseWindow, Destination, MoveTabToWindow, NewWindow, OpenInNewWindow};
use crate::worktrees::{NewWorktreeSession, RemoveWorktree};
use crate::{
    ContinueRecovery, FocusComposer, FocusSearch, Keystone, NewClaude, NewCodex, OpenSettings,
    Quit, RenameSession, RetryRecovery, ToggleSettle, ToggleSidebar,
};
use gpui::{
    Action, AppContext, Context, DummyKeyboardMapper, Entity, FocusHandle, Focusable, IntoElement,
    KeyBinding, KeyBindingContextPredicate, Window,
};
use keystone_core::ViewId;
use keystone_ui::{CommandPalette, CommandPaletteState, PaletteItem};

pub(crate) struct Command {
    /// Stable across releases; the palette row's id.
    pub id: &'static str,
    pub label: &'static str,
    /// GPUI keystrokes. The first is the row's shortcut hint.
    pub keys: &'static [&'static str],
    pub context: Option<&'static str>,
    pub action: fn() -> Box<dyn Action>,
}

/// While the palette is open its shortcuts stay inert, so focus cannot leave it
/// behind an open overlay.
const APP: &str = "!Dialog && !CommandPalette";
const TRANSCRIPT: &str = "Conversation && !Dialog && !CommandPalette";

pub(crate) const COMMANDS: &[Command] = &[
    Command {
        id: "new-codex",
        label: "New Codex session",
        keys: &["ctrl-shift-n"],
        context: Some(APP),
        action: || Box::new(NewCodex),
    },
    Command {
        id: "new-claude",
        label: "New Claude session",
        keys: &["ctrl-n"],
        context: Some(APP),
        action: || Box::new(NewClaude),
    },
    Command {
        id: "toggle-sidebar",
        label: "Toggle sidebar",
        keys: &["ctrl-b"],
        context: Some(APP),
        action: || Box::new(ToggleSidebar),
    },
    Command {
        id: "toggle-files",
        label: "Toggle files",
        keys: &["ctrl-shift-e"],
        context: Some(APP),
        action: || Box::new(ToggleFiles),
    },
    Command {
        id: "toggle-notes",
        label: "Toggle notes",
        keys: &["ctrl-shift-j"],
        context: Some(APP),
        action: || Box::new(ToggleNotes),
    },
    Command {
        id: "toggle-changes",
        label: "Toggle changes",
        keys: &["ctrl-shift-g"],
        context: Some(APP),
        action: || Box::new(ToggleChanges),
    },
    Command {
        id: "copy-pull-request-link",
        label: "Copy pull request link",
        keys: &["ctrl-alt-c"],
        context: Some(APP),
        action: || Box::new(CopyPullRequestLink),
    },
    Command {
        id: "focus-search",
        label: "Focus search",
        keys: &["ctrl-f"],
        context: Some(APP),
        action: || Box::new(FocusSearch),
    },
    Command {
        id: "focus-composer",
        label: "Focus composer",
        keys: &["ctrl-l"],
        context: Some(APP),
        action: || Box::new(FocusComposer),
    },
    Command {
        id: "attach-files",
        label: "Attach files…",
        keys: &["ctrl-shift-u"],
        context: Some(APP),
        action: || Box::new(AttachFiles),
    },
    Command {
        id: "rename-session",
        label: "Rename session",
        keys: &["f2"],
        context: Some(APP),
        action: || Box::new(RenameSession),
    },
    Command {
        id: "hand-off",
        label: "Hand off…",
        keys: &["ctrl-shift-h"],
        context: Some(APP),
        action: || Box::new(HandOff),
    },
    Command {
        id: "second-opinion",
        label: "Second opinion…",
        keys: &["ctrl-alt-o"],
        context: Some(APP),
        action: || Box::new(AskSecondOpinion),
    },
    Command {
        id: "settle-session",
        label: "Settle session",
        keys: &["ctrl-shift-s"],
        context: Some(APP),
        action: || Box::new(ToggleSettle),
    },
    Command {
        id: "continue-recovery",
        label: "Use recovered history",
        keys: &["ctrl-shift-enter"],
        context: Some(APP),
        action: || Box::new(ContinueRecovery),
    },
    Command {
        id: "retry-recovery",
        label: "Retry recovery",
        keys: &["ctrl-shift-r"],
        context: Some(APP),
        action: || Box::new(RetryRecovery),
    },
    Command {
        id: "insert-prompt",
        label: "Insert prompt…",
        keys: &["ctrl-shift-i"],
        context: Some(APP),
        action: || Box::new(InsertPrompt),
    },
    Command {
        id: "open-settings",
        label: "Open settings",
        keys: &["ctrl-,"],
        context: Some(APP),
        action: || Box::new(OpenSettings),
    },
    Command {
        id: "customize-layout",
        label: "Customize layout",
        keys: &["ctrl-shift-l"],
        context: Some("Keystone && !Dialog && !CommandPalette"),
        action: || Box::new(OpenCustomize),
    },
    Command {
        id: "quit",
        label: "Quit",
        keys: &["ctrl-q"],
        context: None,
        action: || Box::new(Quit),
    },
    Command {
        id: "focus-transcript",
        label: "Focus transcript",
        keys: &["ctrl-shift-t"],
        context: Some(APP),
        action: || Box::new(FocusTranscript),
    },
    Command {
        id: "next-transcript",
        label: "Focus next transcript",
        keys: &["f6"],
        context: Some(APP),
        action: || Box::new(NextTranscript),
    },
    Command {
        id: "previous-transcript",
        label: "Focus previous transcript",
        keys: &["shift-f6"],
        context: Some(APP),
        action: || Box::new(PreviousTranscript),
    },
    Command {
        id: "new-tab",
        label: "New tab",
        keys: &["ctrl-t"],
        context: Some(APP),
        action: || Box::new(NewTab),
    },
    Command {
        id: "close-tab",
        label: "Close tab",
        keys: &["ctrl-w"],
        context: Some(APP),
        action: || Box::new(CloseTab),
    },
    Command {
        id: "next-tab",
        label: "Next tab",
        keys: &["ctrl-tab"],
        context: Some(APP),
        action: || Box::new(NextTab),
    },
    Command {
        id: "previous-tab",
        label: "Previous tab",
        keys: &["ctrl-shift-tab"],
        context: Some(APP),
        action: || Box::new(PreviousTab),
    },
    Command {
        id: "split-right",
        label: "Split right",
        keys: &["ctrl-\\"],
        context: Some(APP),
        action: || Box::new(SplitRight),
    },
    Command {
        id: "switch-project",
        label: "Switch project",
        keys: &["ctrl-o"],
        context: Some(APP),
        action: || Box::new(SwitchProject),
    },
    Command {
        id: "open-project",
        label: "Open folder as project",
        keys: &["ctrl-shift-o"],
        context: Some(APP),
        action: || Box::new(OpenProject),
    },
    Command {
        id: "new-worktree-session",
        label: "New session in worktree…",
        keys: &["ctrl-alt-g"],
        context: Some(APP),
        action: || Box::new(NewWorktreeSession),
    },
    Command {
        id: "remove-worktree",
        label: "Remove worktree…",
        keys: &["ctrl-alt-shift-g"],
        context: Some(APP),
        action: || Box::new(RemoveWorktree),
    },
    Command {
        id: "new-window",
        label: "New window",
        keys: &["ctrl-alt-n"],
        context: Some(APP),
        action: || Box::new(NewWindow),
    },
    Command {
        id: "quick-composer",
        label: "Quick composer",
        keys: &["ctrl-alt-space"],
        context: Some(APP),
        action: || Box::new(OpenQuickComposer),
    },
    Command {
        id: "close-window",
        label: "Close window",
        keys: &["ctrl-shift-w"],
        context: Some(APP),
        action: || Box::new(CloseWindow),
    },
    Command {
        id: "open-in-new-window",
        label: "Open session in new window",
        keys: &["ctrl-alt-shift-n"],
        context: Some(APP),
        action: || Box::new(OpenInNewWindow),
    },
    Command {
        id: "move-tab-to-window",
        label: "Move tab to window…",
        keys: &["ctrl-alt-m"],
        context: Some(APP),
        action: || Box::new(MoveTabToWindow),
    },
    Command {
        id: "transcript-previous-row",
        label: "Transcript: previous row",
        keys: &["up"],
        context: Some(TRANSCRIPT),
        action: || Box::new(PreviousRow),
    },
    Command {
        id: "transcript-next-row",
        label: "Transcript: next row",
        keys: &["down"],
        context: Some(TRANSCRIPT),
        action: || Box::new(NextRow),
    },
    Command {
        id: "transcript-first-row",
        label: "Transcript: first row",
        keys: &["home"],
        context: Some(TRANSCRIPT),
        action: || Box::new(FirstRow),
    },
    Command {
        id: "transcript-last-row",
        label: "Transcript: last row",
        keys: &["end"],
        context: Some(TRANSCRIPT),
        action: || Box::new(LastRow),
    },
    Command {
        id: "transcript-next-action",
        label: "Transcript: next action",
        keys: &["tab"],
        context: Some(TRANSCRIPT),
        action: || Box::new(NextAction),
    },
    Command {
        id: "transcript-previous-action",
        label: "Transcript: previous action",
        keys: &["shift-tab"],
        context: Some(TRANSCRIPT),
        action: || Box::new(PreviousAction),
    },
    Command {
        id: "transcript-copy",
        label: "Transcript: copy focused content",
        keys: &["ctrl-shift-c"],
        context: Some(TRANSCRIPT),
        action: || Box::new(CopyTranscript),
    },
    Command {
        id: "fork-from-here",
        label: "Transcript: fork from row",
        keys: &["ctrl-shift-f"],
        context: Some(TRANSCRIPT),
        action: || Box::new(ForkFromHere),
    },
    Command {
        id: "transcript-focus-latest",
        label: "Transcript: jump to latest",
        keys: &["alt-end"],
        context: Some(TRANSCRIPT),
        action: || Box::new(FocusLatest),
    },
    Command {
        id: "transcript-leave",
        label: "Leave transcript for composer",
        keys: &["escape"],
        context: Some(TRANSCRIPT),
        action: || Box::new(LeaveTranscript),
    },
];

pub(crate) fn bindings() -> Vec<KeyBinding> {
    COMMANDS
        .iter()
        .flat_map(|command| {
            command.keys.iter().map(|keys| {
                let context = command
                    .context
                    .map(|context| KeyBindingContextPredicate::parse(context).unwrap().into());
                KeyBinding::load(
                    keys,
                    (command.action)(),
                    context,
                    false,
                    None,
                    &DummyKeyboardMapper,
                )
                .unwrap()
            })
        })
        .collect()
}

pub(crate) fn palette_items() -> Vec<PaletteItem> {
    COMMANDS
        .iter()
        .map(|command| {
            let item = PaletteItem::new(command.id, command.label);
            match command.keys.first() {
                Some(keys) => item.kbd(kbd_text(keys)),
                None => item,
            }
        })
        .collect()
}

pub(crate) fn action(id: &str) -> Option<Box<dyn Action>> {
    COMMANDS
        .iter()
        .find(|command| command.id == id)
        .map(|command| (command.action)())
}

/// `ctrl-shift-n` as `Ctrl+Shift+N`. Chords separated by spaces stay separated.
fn kbd_text(keys: &str) -> String {
    keys.split_whitespace()
        .map(|stroke| {
            stroke
                .split('-')
                .map(|part| {
                    let mut chars = part.chars();
                    match chars.next() {
                        Some(first) => first.to_uppercase().chain(chars).collect(),
                        None => String::new(),
                    }
                })
                .collect::<Vec<_>>()
                .join("+")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The open palette and the focus it returns to when it closes.
pub(crate) struct Palette {
    pub(crate) state: Entity<CommandPaletteState>,
    pub(crate) restore: Option<FocusHandle>,
    /// The tab whose destination the palette lists, instead of commands.
    moving: Option<ViewId>,
}

/// The palette row that moves a tab into a new window.
const NEW_WINDOW_ROW: &str = "move-to-new-window";

impl Keystone {
    pub(crate) fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            return self.close_palette(window, cx);
        }
        self.new_menu = false;
        self.filter_menu = false;
        self.approval_menu = false;
        self.model_menu = None;
        self.open_palette(palette_items(), None, window, cx);
    }

    fn open_palette(
        &mut self,
        items: Vec<PaletteItem>,
        moving: Option<ViewId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let restore = self
            .projects
            .take()
            .and_then(|list| list.restore)
            .or_else(|| self.prompt_list.take().and_then(|list| list.restore))
            .or_else(|| window.focused(cx));
        let state = cx.new(|cx| CommandPaletteState::new(items, cx));
        state.focus_handle(cx).focus(window, cx);
        self.palette = Some(Palette {
            state,
            restore,
            moving,
        });
        self.refresh(cx);
    }

    /// Lists the windows the active tab can move to: every other window on this project,
    /// and a new one.
    pub(crate) fn open_move_tab_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_palette(window, cx);
        let Some(view) = self
            .active_view(cx)
            .filter(|&view| self.view_session(view, cx).is_some())
        else {
            self.error = Some("No session open.".into());
            return self.refresh(cx);
        };
        let this = cx.entity();
        let project = self.presentation.project.as_ref().ok();
        let mut items: Vec<PaletteItem> = crate::open_windows(cx)
            .into_iter()
            .filter(|other| *other != this)
            .map(|other| other.read(cx))
            .filter(|other| other.presentation.project.as_ref().ok() == project)
            .map(|other| {
                PaletteItem::new(
                    format!("move-to-window-{}", other.window_id.0),
                    windows::title(other.window_id),
                )
            })
            .collect();
        items.push(PaletteItem::new(NEW_WINDOW_ROW, "New window"));
        self.open_palette(items, Some(view), window, cx);
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(palette) = self.palette.take() else {
            return;
        };
        palette
            .restore
            .unwrap_or_else(|| self.focus.clone())
            .focus(window, cx);
        self.refresh(cx);
    }

    /// Closes first so the action reaches the restored focus, as its shortcut would.
    fn confirm_palette(&mut self, item: &PaletteItem, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.palette.as_ref().and_then(|palette| palette.moving) {
            self.close_palette(window, cx);
            let source = cx.entity();
            let destination = if item.id == NEW_WINDOW_ROW {
                Some(Destination::NewWindow)
            } else {
                crate::open_windows(cx)
                    .into_iter()
                    .filter(|other| *other != source)
                    .find(|other| {
                        format!("move-to-window-{}", other.read(cx).window_id.0) == item.id
                    })
                    .map(|target| Destination::Window(target, windows::Landing::Active))
            };
            if let Some(destination) = destination {
                cx.defer(move |cx| windows::move_tab(&source, view, destination, &source, cx));
            }
            return;
        }
        let action = action(&item.id);
        self.close_palette(window, cx);
        if let Some(action) = action {
            window.dispatch_action(action, cx);
        }
    }

    pub(crate) fn palette_overlay(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let palette = self.palette.as_ref()?;
        let dismiss = self.scoped_listener(cx, |this, _: &(), window, cx| {
            this.close_palette(window, cx)
        });
        Some(
            CommandPalette::new("palette", &palette.state)
                .on_confirm(
                    self.scoped_listener(cx, |this, item: &PaletteItem, window, cx| {
                        this.confirm_palette(item, window, cx)
                    }),
                )
                .on_dismiss(move |window, cx| dismiss(&(), window, cx)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_command_has_a_bound_action() {
        let bindings = bindings();
        let mut ids = HashSet::new();
        let mut actions = HashSet::new();
        for command in COMMANDS {
            assert!(!command.keys.is_empty(), "{} has no key", command.id);
            assert!(ids.insert(command.id), "{} is listed twice", command.id);
            let name = (command.action)().name();
            assert!(actions.insert(name), "{name} backs two rows");
            assert!(
                bindings
                    .iter()
                    .any(|binding| binding.action().name() == name),
                "{name} has no binding"
            );
        }
    }

    /// Two open branches can each pick the same free chord; merging both must fail here.
    #[test]
    fn no_two_commands_share_a_key_in_one_context() {
        let mut keys = HashSet::new();
        for command in COMMANDS {
            for key in command.keys {
                assert!(
                    keys.insert((command.context, *key)),
                    "{key} is bound twice, the second time to {}",
                    command.id
                );
            }
        }
    }

    #[test]
    fn palette_kbd_matches_binding() {
        let bindings = bindings();
        for (command, item) in COMMANDS.iter().zip(palette_items()) {
            let name = (command.action)().name();
            let binding = bindings
                .iter()
                .find(|binding| binding.action().name() == name)
                .unwrap();
            let keys = binding
                .keystrokes()
                .iter()
                .map(|stroke| stroke.inner().unparse())
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(item.id.as_ref(), command.id);
            assert_eq!(item.kbd.as_deref(), Some(kbd_text(&keys).as_str()));
        }
    }

    #[test]
    fn kbd_text_names_each_key() {
        for (keys, text) in [
            ("ctrl-shift-n", "Ctrl+Shift+N"),
            ("ctrl-b", "Ctrl+B"),
            ("f2", "F2"),
            ("alt-enter", "Alt+Enter"),
            ("ctrl-k ctrl-s", "Ctrl+K Ctrl+S"),
        ] {
            assert_eq!(kbd_text(keys), text);
        }
    }
}
