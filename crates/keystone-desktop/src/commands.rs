//! The commands the palette lists. Their key bindings come from the same table, so a
//! row's shortcut hint cannot drift from the binding that runs it.

use crate::app_state::setup::{CancelSetup, EditSetupScript, RetrySetup, SkipSetup};
use crate::attachments::AttachFiles;
use crate::changes::{CopyPullRequestLink, ToggleChanges};
use crate::customization::OpenCustomize;
use crate::diff_view::{ViewChanges, ViewUncommitted};
use crate::excerpt::{SEND_SELECTION, SendSelection, Taken};
use crate::file_view::{SAVE_FILE, Save};
use crate::files::ToggleFiles;
use crate::fork::ForkFromHere;
use crate::handoff::HandOff;
use crate::import::ImportSessions;
use crate::keymap::{self, Keymap};
use crate::model_selector::{NextEffort, NextModel, ToggleFastMode};
use crate::notes::ToggleNotes;
use crate::projects::{ChangeProjectColor, OpenProject, SwitchProject};
use crate::prompts::InsertPrompt;
use crate::pull_request::TogglePullRequest;
use crate::quick::OpenQuickComposer;
use crate::restore::RestoreToHere;
use crate::screenshot::CaptureScreenshot;
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
    Action, App, AppContext, Context, DummyKeyboardMapper, Entity, FocusHandle, Focusable,
    IntoElement, KeyBinding, KeyBindingContextPredicate, Window,
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
/// The Files editor, where Save runs.
const FILE_EDITOR: &str = "FileView && !Dialog && !CommandPalette";

pub(crate) const COMMANDS: &[Command] = &[
    Command {
        id: "new-codex",
        label: "New Codex session",
        keys: &["secondary-shift-n"],
        context: Some(APP),
        action: || Box::new(NewCodex),
    },
    Command {
        id: "new-claude",
        label: "New Claude session",
        keys: &["secondary-n"],
        context: Some(APP),
        action: || Box::new(NewClaude),
    },
    Command {
        id: "toggle-sidebar",
        label: "Toggle sidebar",
        keys: &["secondary-b"],
        context: Some(APP),
        action: || Box::new(ToggleSidebar),
    },
    Command {
        id: "toggle-files",
        label: "Toggle files",
        keys: &["secondary-shift-e"],
        context: Some(APP),
        action: || Box::new(ToggleFiles),
    },
    Command {
        id: "toggle-notes",
        label: "Toggle notes",
        keys: &["secondary-shift-j"],
        context: Some(APP),
        action: || Box::new(ToggleNotes),
    },
    Command {
        id: "view-uncommitted",
        label: "View uncommitted changes",
        keys: &["secondary-alt-u"],
        context: Some(APP),
        action: || Box::new(ViewUncommitted),
    },
    Command {
        id: "toggle-changes",
        label: "Toggle changes",
        keys: &["secondary-shift-g"],
        context: Some(APP),
        action: || Box::new(ToggleChanges),
    },
    Command {
        id: "toggle-pull-request",
        label: "Toggle pull request",
        keys: &["secondary-alt-p"],
        context: Some(APP),
        action: || Box::new(TogglePullRequest),
    },
    Command {
        id: "copy-pull-request-link",
        label: "Copy pull request link",
        keys: &["secondary-alt-c"],
        context: Some(APP),
        action: || Box::new(CopyPullRequestLink),
    },
    Command {
        id: "focus-search",
        label: "Focus search",
        keys: &["secondary-f"],
        context: Some(APP),
        action: || Box::new(FocusSearch),
    },
    Command {
        id: SEND_SELECTION,
        label: "Send selection to agent",
        keys: &["secondary-shift-a"],
        context: Some(APP),
        action: || Box::new(SendSelection),
    },
    Command {
        id: SAVE_FILE,
        label: "Save file",
        keys: &["secondary-s"],
        context: Some(FILE_EDITOR),
        action: || Box::new(Save),
    },
    Command {
        id: "focus-composer",
        label: "Focus composer",
        keys: &["secondary-l"],
        context: Some(APP),
        action: || Box::new(FocusComposer),
    },
    Command {
        id: "attach-files",
        label: "Attach files…",
        keys: &["secondary-shift-u"],
        context: Some(APP),
        action: || Box::new(AttachFiles),
    },
    Command {
        id: "import-sessions",
        label: "Import sessions…",
        keys: &["secondary-alt-i"],
        context: Some(APP),
        action: || Box::new(ImportSessions),
    },
    Command {
        id: "capture-screenshot",
        label: "Capture screenshot",
        keys: &["secondary-alt-s"],
        context: Some(APP),
        action: || Box::new(CaptureScreenshot),
    },
    Command {
        id: "next-effort",
        label: "Next effort level",
        keys: &["secondary-alt-e"],
        context: Some(APP),
        action: || Box::new(NextEffort),
    },
    Command {
        id: "toggle-fast-mode",
        label: "Toggle fast mode",
        keys: &["secondary-alt-f"],
        context: Some(APP),
        action: || Box::new(ToggleFastMode),
    },
    Command {
        id: "next-model",
        label: "Next model",
        keys: &[],
        context: Some(APP),
        action: || Box::new(NextModel),
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
        keys: &["secondary-shift-h"],
        context: Some(APP),
        action: || Box::new(HandOff),
    },
    Command {
        id: "second-opinion",
        label: "Second opinion…",
        keys: &["secondary-alt-o"],
        context: Some(APP),
        action: || Box::new(AskSecondOpinion),
    },
    Command {
        id: "settle-session",
        label: "Settle session",
        keys: &["secondary-shift-s"],
        context: Some(APP),
        action: || Box::new(ToggleSettle),
    },
    Command {
        id: "continue-recovery",
        label: "Use recovered history",
        keys: &["secondary-shift-enter"],
        context: Some(APP),
        action: || Box::new(ContinueRecovery),
    },
    Command {
        id: "retry-recovery",
        label: "Retry recovery",
        keys: &["secondary-shift-r"],
        context: Some(APP),
        action: || Box::new(RetryRecovery),
    },
    Command {
        id: "retry-setup",
        label: "Retry setup script",
        keys: &["secondary-alt-r"],
        context: Some(APP),
        action: || Box::new(RetrySetup),
    },
    Command {
        id: "skip-setup",
        label: "Skip setup script",
        keys: &["secondary-alt-shift-s"],
        context: Some(APP),
        action: || Box::new(SkipSetup),
    },
    Command {
        id: "cancel-setup",
        label: "Cancel setup script",
        keys: &["secondary-alt-x"],
        context: Some(APP),
        action: || Box::new(CancelSetup),
    },
    Command {
        id: "edit-setup-script",
        label: "Edit worktree setup script…",
        keys: &["secondary-alt-shift-e"],
        context: Some(APP),
        action: || Box::new(EditSetupScript),
    },
    Command {
        id: "insert-prompt",
        label: "Insert prompt…",
        keys: &["secondary-shift-i"],
        context: Some(APP),
        action: || Box::new(InsertPrompt),
    },
    Command {
        id: "open-settings",
        label: "Open settings",
        keys: &["secondary-,"],
        context: Some(APP),
        action: || Box::new(OpenSettings),
    },
    Command {
        id: "customize-layout",
        label: "Customize layout",
        keys: &["secondary-shift-l"],
        context: Some("Keystone && !Dialog && !CommandPalette"),
        action: || Box::new(OpenCustomize),
    },
    Command {
        id: "quit",
        label: "Quit",
        keys: &["secondary-q"],
        context: None,
        action: || Box::new(Quit),
    },
    Command {
        id: "focus-transcript",
        label: "Focus transcript",
        keys: &["secondary-shift-t"],
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
        keys: &["secondary-t"],
        context: Some(APP),
        action: || Box::new(NewTab),
    },
    Command {
        id: "close-tab",
        label: "Close tab",
        keys: &["secondary-w"],
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
        keys: &["secondary-\\"],
        context: Some(APP),
        action: || Box::new(SplitRight),
    },
    Command {
        id: "switch-project",
        label: "Switch project",
        keys: &["secondary-o"],
        context: Some(APP),
        action: || Box::new(SwitchProject),
    },
    Command {
        id: "open-project",
        label: "Open folder as project",
        keys: &["secondary-shift-o"],
        context: Some(APP),
        action: || Box::new(OpenProject),
    },
    Command {
        id: "change-project-color",
        label: "Change project color…",
        keys: &["secondary-alt-k"],
        context: Some(APP),
        action: || Box::new(ChangeProjectColor),
    },
    Command {
        id: "new-worktree-session",
        label: "New session in worktree…",
        keys: &["secondary-alt-g"],
        context: Some(APP),
        action: || Box::new(NewWorktreeSession),
    },
    Command {
        id: "remove-worktree",
        label: "Remove worktree…",
        keys: &["secondary-alt-shift-g"],
        context: Some(APP),
        action: || Box::new(RemoveWorktree),
    },
    Command {
        id: "new-window",
        label: "New window",
        keys: &["secondary-alt-n"],
        context: Some(APP),
        action: || Box::new(NewWindow),
    },
    Command {
        id: "quick-composer",
        label: "Quick composer",
        keys: &["secondary-alt-space"],
        context: Some(APP),
        action: || Box::new(OpenQuickComposer),
    },
    Command {
        id: "close-window",
        label: "Close window",
        keys: &["secondary-shift-w"],
        context: Some(APP),
        action: || Box::new(CloseWindow),
    },
    Command {
        id: "open-in-new-window",
        label: "Open session in new window",
        keys: &["secondary-alt-shift-n"],
        context: Some(APP),
        action: || Box::new(OpenInNewWindow),
    },
    Command {
        id: "move-tab-to-window",
        label: "Move tab to window…",
        keys: &["secondary-alt-m"],
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
        keys: &["secondary-shift-c"],
        context: Some(TRANSCRIPT),
        action: || Box::new(CopyTranscript),
    },
    Command {
        id: "fork-from-here",
        label: "Transcript: fork from row",
        keys: &["secondary-shift-f"],
        context: Some(TRANSCRIPT),
        action: || Box::new(ForkFromHere),
    },
    Command {
        id: "restore-to-here",
        label: "Transcript: restore to the focused turn",
        keys: &["secondary-shift-z"],
        context: Some(TRANSCRIPT),
        action: || Box::new(RestoreToHere),
    },
    Command {
        id: "view-changes",
        label: "Transcript: view the focused turn's changes",
        keys: &["secondary-shift-d"],
        context: Some(TRANSCRIPT),
        action: || Box::new(ViewChanges),
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

pub(crate) fn bindings(keymap: &Keymap) -> Vec<KeyBinding> {
    COMMANDS
        .iter()
        .flat_map(|command| {
            let keys: &[&str] = if keymap.overridden(command.id) {
                &[keymap.keys(command)]
            } else {
                command.keys
            };
            keys.iter()
                .map(|keys| {
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
                .collect::<Vec<_>>()
        })
        .collect()
}

pub(crate) fn palette_items(keymap: &Keymap) -> Vec<PaletteItem> {
    COMMANDS
        .iter()
        .map(|command| {
            let item = PaletteItem::new(command.id, command.label);
            match keymap.keys(command) {
                "" => item,
                keys => item.kbd(keystone_ui::shortcut_text(keys)),
            }
        })
        .collect()
}

/// The shortcut hint for command `id`, as the user bound it. An id that names no
/// command shows no hint; `command_hints_follow_the_users_binding` catches one.
pub(crate) fn hint(id: &str, cx: &App) -> String {
    let command = keymap::command(id);
    debug_assert!(command.is_some(), "{id} is not a command");
    command.map_or_else(String::new, |command| {
        keystone_ui::shortcut_text(keymap::current(cx).keys(command))
    })
}

pub(crate) fn action(id: &str) -> Option<Box<dyn Action>> {
    COMMANDS
        .iter()
        .find(|command| command.id == id)
        .map(|command| (command.action)())
}

/// The open palette and the focus it returns to when it closes.
pub(crate) struct Palette {
    pub(crate) state: Entity<CommandPaletteState>,
    pub(crate) restore: Option<FocusHandle>,
    listing: Listing,
    /// What was selected when it opened. A reply's selection clears once focus leaves it.
    selection: Option<Taken>,
}

/// What the palette lists.
pub(crate) enum Listing {
    Commands,
    /// The windows this tab can move to.
    Destinations(ViewId),
    /// The colors the window's project can take.
    ProjectColors,
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
        self.attach.menu = false;
        self.model_menu = None;
        let items = palette_items(keymap::current(cx));
        self.open_palette(items, Listing::Commands, window, cx);
    }

    pub(crate) fn open_palette(
        &mut self,
        items: Vec<PaletteItem>,
        listing: Listing,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let restore = self
            .projects
            .take()
            .and_then(|list| list.restore)
            .or_else(|| self.prompt_list.take().and_then(|list| list.restore))
            .or_else(|| window.focused(cx));
        let selection = matches!(listing, Listing::Commands)
            .then(|| self.take_selection(window, cx))
            .flatten();
        let state = cx.new(|cx| match listing {
            Listing::ProjectColors => CommandPaletteState::with_placeholder(items, "Color", cx),
            Listing::Commands | Listing::Destinations(_) => CommandPaletteState::new(items, cx),
        });
        state.focus_handle(cx).focus(window, cx);
        self.palette = Some(Palette {
            state,
            restore,
            listing,
            selection,
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
        self.open_palette(items, Listing::Destinations(view), window, cx);
    }

    pub(crate) fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        let Some(palette) = self.palette.as_ref() else {
            return;
        };
        if let Listing::ProjectColors = palette.listing {
            return self.confirm_project_color(item, window, cx);
        }
        if let Listing::Destinations(view) = palette.listing {
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
        if item.id == SEND_SELECTION {
            let selection = palette.selection.clone();
            self.close_palette(window, cx);
            if let Some(selection) = selection {
                self.send_excerpt(&selection, window, cx);
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
        let bindings = bindings(&Keymap::parse(&serde_json::Value::Null));
        let mut ids = HashSet::new();
        let mut actions = HashSet::new();
        for command in COMMANDS {
            assert!(ids.insert(command.id), "{} is listed twice", command.id);
            let name = (command.action)().name();
            assert!(actions.insert(name), "{name} backs two rows");
            // A palette-only command has no key.
            assert_eq!(
                bindings
                    .iter()
                    .any(|binding| binding.action().name() == name),
                !command.keys.is_empty(),
                "{name}'s binding"
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
        for saved in [
            serde_json::Value::Null,
            serde_json::json!({"new-claude": "secondary-j", "next-model": "secondary-alt-shift-m"}),
        ] {
            palette_kbd_matches(&Keymap::parse(&saved));
        }
    }

    fn palette_kbd_matches(keymap: &Keymap) {
        let bindings = bindings(keymap);
        for (command, item) in COMMANDS.iter().zip(palette_items(keymap)) {
            let name = (command.action)().name();
            let Some(binding) = bindings
                .iter()
                .find(|binding| binding.action().name() == name)
            else {
                assert_eq!(item.kbd, None, "{name}");
                continue;
            };
            let keys = binding
                .keystrokes()
                .iter()
                .map(|stroke| stroke.inner().unparse())
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(item.id.as_ref(), command.id);
            assert_eq!(
                item.kbd.as_deref(),
                Some(keystone_ui::shortcut_text(&keys).as_str())
            );
        }
    }

    /// The string literal passed first to each `call` in `text`.
    fn literals<'a>(text: &'a str, call: &'static str) -> impl Iterator<Item = &'a str> {
        text.match_indices(call).filter_map(move |(at, _)| {
            if text[..at].ends_with('"') {
                return None;
            }
            let rest = text[at + call.len()..].trim_start();
            rest.strip_prefix('"')
                .and_then(|rest| rest.split('"').next())
        })
    }

    /// The string literal passed second to each `call` in `text`, such as the keys a
    /// tooltip shows beside its label.
    fn second_literals<'a>(text: &'a str, call: &'static str) -> impl Iterator<Item = &'a str> {
        text.match_indices(call).filter_map(move |(at, _)| {
            let args = &text[at + call.len()..];
            let (mut depth, mut quoted) = (0, false);
            let comma = args.char_indices().find_map(|(index, character)| {
                match character {
                    '"' => quoted = !quoted,
                    '(' | '[' | '{' if !quoted => depth += 1,
                    ')' | ']' | '}' if !quoted => {
                        if depth == 0 {
                            return Some(None);
                        }
                        depth -= 1;
                    }
                    ',' if !quoted && depth == 0 => return Some(Some(index)),
                    _ => {}
                }
                None
            })??;
            args[comma + 1..]
                .trim_start()
                .strip_prefix('"')
                .and_then(|rest| rest.split('"').next())
        })
    }

    /// A hint written as a command's default keys would go stale once the user rebinds
    /// it, and a hint naming no command would show nothing.
    #[test]
    fn command_hints_follow_the_users_binding() {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stale = Vec::new();
        for file in std::fs::read_dir(source).unwrap() {
            let path = file.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if !name.ends_with(".rs") || name.ends_with("_tests.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            for keys in literals(&text, "shortcut_text(") {
                if let Some(command) = COMMANDS.iter().find(|c| c.keys.contains(&keys)) {
                    stale.push(format!("{name}: use commands::hint(\"{}\")", command.id));
                }
            }
            for shown in second_literals(&text, "with_key(") {
                if let Some(command) = COMMANDS.iter().find(|c| {
                    c.keys.first().map(|keys| keystone_ui::shortcut_text(keys))
                        == Some(shown.to_owned())
                }) {
                    stale.push(format!("{name}: use commands::hint(\"{}\")", command.id));
                }
            }
            for id in literals(&text, "commands::hint(") {
                if keymap::command(id).is_none() {
                    stale.push(format!("{name}: \"{id}\" is not a command"));
                }
            }
        }
        assert!(stale.is_empty(), "{}", stale.join("\n"));
    }
}
