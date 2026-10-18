//! The command palette page: a button that opens a palette of 2,000 commands, and a
//! status line that names the last confirm or dismiss.

use gpui::{Context, Entity, IntoElement, Render, SharedString, Window, div, prelude::*};
use keystone_ui::prelude::*;
use std::collections::HashSet;

const COUNT: usize = 2_000;

const NAMED: [(&str, Option<&str>, Option<&str>); 30] = [
    (
        "New session",
        Some("Starts with the default provider"),
        Some("Ctrl+N"),
    ),
    ("New Claude session", None, Some("Ctrl+Alt+C")),
    ("New Codex session", None, Some("Ctrl+Alt+X")),
    ("Open settings", None, Some("Ctrl+,")),
    ("Toggle sidebar", None, Some("Ctrl+B")),
    ("Set theme: Light", None, None),
    ("Set theme: Dark", None, None),
    ("Set density: Compact", None, None),
    ("Set density: Default", None, None),
    ("Set density: Spacious", None, None),
    ("Focus composer", None, Some("Ctrl+L")),
    ("Send message", None, Some("Enter")),
    (
        "Stop turn",
        Some("Interrupts the running agent"),
        Some("Ctrl+C"),
    ),
    ("Approve request", None, Some("Ctrl+Enter")),
    ("Deny request", None, None),
    (
        "Approval mode: Ask first",
        Some("Asks before commands not known to be safe"),
        None,
    ),
    (
        "Approval mode: Full access",
        Some("Runs commands without asking"),
        None,
    ),
    ("Switch model", None, Some("Ctrl+M")),
    ("Open project", None, Some("Ctrl+O")),
    ("Close tab", None, Some("Ctrl+W")),
    ("Next tab", None, Some("Ctrl+Tab")),
    ("Previous tab", None, Some("Ctrl+Shift+Tab")),
    ("Split right", None, None),
    ("Split down", None, None),
    ("Copy last reply", None, None),
    ("Show keyboard shortcuts", None, Some("Ctrl+/")),
    ("Reload window", None, Some("Ctrl+R")),
    ("Quit Keystone", None, Some("Ctrl+Q")),
    ("Check for updates", None, None),
    ("Report an issue", Some("Opens the issue tracker"), None),
];

const VERBS: [&str; 20] = [
    "Open",
    "Close",
    "Rename",
    "Delete",
    "Duplicate",
    "Move",
    "Copy",
    "Pin",
    "Archive",
    "Restore",
    "Export",
    "Import",
    "Reload",
    "Show",
    "Hide",
    "Search",
    "Filter",
    "Sort",
    "Reset",
    "Inspect",
];

const NOUNS: [&str; 50] = [
    "project",
    "worktree",
    "branch",
    "commit",
    "terminal",
    "file",
    "folder",
    "tab",
    "pane",
    "composer",
    "transcript",
    "keymap",
    "model",
    "provider",
    "approval",
    "diff",
    "review",
    "comment",
    "issue",
    "pull request",
    "workspace",
    "layout",
    "split",
    "notification",
    "log",
    "trace",
    "profile",
    "snippet",
    "bookmark",
    "history",
    "attachment",
    "screenshot",
    "agent",
    "task",
    "schedule",
    "draft",
    "reply",
    "prompt",
    "tool call",
    "server",
    "plugin",
    "extension",
    "token",
    "secret",
    "remote",
    "stash",
    "tag",
    "release",
    "pipeline",
    "deployment",
];

const SCOPES: [&str; 2] = ["", " in current project"];

fn commands() -> Vec<PaletteItem> {
    let named = NAMED.iter().map(|&(label, detail, kbd)| {
        let item = PaletteItem::new(label, label);
        let item = match detail {
            Some(detail) => item.detail(detail),
            None => item,
        };
        match kbd {
            Some(kbd) => item.kbd(kbd),
            None => item,
        }
    });
    let generated = SCOPES.iter().flat_map(|scope| {
        NOUNS.iter().flat_map(move |noun| {
            VERBS.iter().map(move |verb| {
                let label = format!("{verb} {noun}{scope}");
                PaletteItem::new(label.clone(), label).detail(format!("{noun} commands"))
            })
        })
    });
    let mut seen = HashSet::new();
    let items: Vec<_> = named
        .chain(generated)
        .filter(|item| seen.insert(item.label.clone()))
        .take(COUNT)
        .collect();
    assert_eq!(items.len(), COUNT);
    items
}

pub struct PalettePage {
    palette: Option<Entity<CommandPaletteState>>,
    status: SharedString,
}

impl PalettePage {
    pub fn new() -> Self {
        Self {
            palette: None,
            status: "palette closed".into(),
        }
    }

    fn close(&mut self, status: SharedString, cx: &mut Context<Self>) {
        self.palette = None;
        self.status = status;
        cx.notify();
    }
}

impl Render for PalettePage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        v_flex()
            .px_6()
            .pb_6()
            .gap_3()
            .child(
                h_flex().child(
                    Button::new("gallery-palette-open", "Open command palette")
                        .style(ButtonStyle::Subtle)
                        .icon(IconName::Search)
                        .debug_selector(|| "gallery-palette-open".into())
                        .on_click(cx.listener(|this, _, window, cx| {
                            let state = cx.new(|cx| CommandPaletteState::new(commands(), cx));
                            window.focus(&gpui::Focusable::focus_handle(state.read(cx), cx), cx);
                            this.palette = Some(state);
                            cx.notify();
                        })),
                ),
            )
            .child(
                div()
                    .automation_id("gallery-palette-status")
                    .child(Label::new(self.status.clone()).color(Color::Muted)),
            )
            .when_some(self.palette.clone(), |d, state| {
                d.child(
                    CommandPalette::new("gallery-palette", &state)
                        .on_confirm(cx.listener(|this, item: &PaletteItem, _, cx| {
                            this.close(format!("confirmed {}", item.label).into(), cx)
                        }))
                        .on_dismiss(move |_, cx| {
                            view.update(cx, |this, cx| this.close("dismissed".into(), cx))
                        }),
                )
            })
    }
}
