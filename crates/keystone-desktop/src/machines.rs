//! Machines: Settings → Machines, where a machine is linked by its SSH destination and
//! each one's link state shows, and the palette that starts a session in a folder on
//! one. The link's state is one [`LinkStatus`], which the sidebar, the session header
//! and the composer all read. See `docs/design/remote.md`.

use crate::Keystone;
use crate::app_state::AppState;
use crate::settings_page::SettingsPage;
use gpui::{
    AnyElement, App, AppContext, Context, Entity, FocusHandle, Focusable, FontWeight, KeyDownEvent,
    Window, div, prelude::*, px,
};
use keystone_agent::remote::Status as LinkStatus;
use keystone_core::{Machine, MachineId, Provider, RemoteFolder, WindowId};
use keystone_ui::PaletteItem;
use keystone_ui::prelude::*;
use keystone_ui::text_input::{InputMode, TextInput};
use std::cell::RefCell;
use std::collections::HashMap;

gpui::actions!(keystone, [LinkMachine, NewRemoteSession]);

#[cfg(all(test, feature = "ui-tests"))]
#[path = "machines_tests.rs"]
mod tests;

/// The palette row that opens Settings → Machines from the machine list.
const LINK_ROW: &str = "link-machine";
const MACHINE_ROW: &str = "machine:";
const FOLDER_ROW: &str = "remote-folder:";

/// The word and the mark color for a link's state. Never "Connected" unless it is.
pub(crate) fn state_word(status: &LinkStatus) -> (&'static str, Color) {
    match status {
        LinkStatus::Connected => ("Connected", Color::Success),
        LinkStatus::Connecting => ("Connecting", Color::Faint),
        LinkStatus::Reconnecting => ("Reconnecting", Color::Warning),
        LinkStatus::Failed(_) => ("Can't connect", Color::Danger),
    }
}

/// Why Send is off in a session on a machine in `status`, or `None` while it's on.
pub(crate) fn offline_reason(label: &str, status: &LinkStatus) -> Option<String> {
    match status {
        LinkStatus::Connected => None,
        LinkStatus::Connecting => Some(format!("Connecting to {label}")),
        LinkStatus::Reconnecting => Some(format!("Reconnecting to {label}")),
        LinkStatus::Failed(_) => Some(format!("Can't connect to {label}")),
    }
}

/// A machine's name after a dot in its state's color. With `word`, the state's word
/// follows, as the session header shows it; the sidebar row has room for the dot only.
/// The name is what gives way when the space runs out. Quiet: the color is on the dot.
pub(crate) fn mark(
    label: &str,
    status: &LinkStatus,
    word: bool,
    id: String,
    cx: &App,
) -> gpui::Div {
    let (state, color) = state_word(status);
    h_flex()
        .min_w_0()
        .flex_shrink(1.)
        .gap(px(5.))
        .automation_id(id)
        .child(
            div()
                .flex_none()
                .size(px(6.))
                .rounded_full()
                .bg(color.to_hsla(cx)),
        )
        .child(
            div().min_w_0().max_w(px(120.)).child(
                Label::new(label.to_owned())
                    .size(TextSize::Sm)
                    .color(Color::Muted)
                    .truncate(),
            ),
        )
        .when(word, |d| {
            d.child(
                div()
                    .flex_none()
                    .child(Label::new(state).size(TextSize::Sm).color(Color::Faint)),
            )
        })
}

/// The Send button's tooltip in a session. It follows the link's state while it
/// shows, so it never names a state the machine has left.
pub(crate) struct SendTooltip {
    state: Entity<AppState>,
    session: u64,
    _changed: gpui::Subscription,
}

impl SendTooltip {
    pub(crate) fn builder(
        state: Entity<AppState>,
        session: u64,
    ) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static {
        move |_, cx| {
            let state = state.clone();
            cx.new(|cx| SendTooltip {
                _changed: cx.observe(&state, |_, _, cx| cx.notify()),
                state,
                session,
            })
            .into()
        }
    }
}

impl gpui::Render for SendTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);
        let running = state
            .conversations()
            .get(&self.session)
            .is_some_and(|c| c.running);
        let tooltip = match state.offline(self.session) {
            Some(reason) => Tooltip::text(reason),
            None => Tooltip::with_key(
                if running { "Steer" } else { "Send" },
                keystone_ui::shortcut_text("secondary-enter"),
            ),
        };
        div().child(gpui::AnyView::from(cx.new(|_| tooltip)))
    }
}

/// The section's own state, kept while the settings page is open.
pub(crate) struct MachinesPage {
    destination: Entity<TextInput>,
    link: FocusHandle,
    /// Focus for each row's buttons, made as rows first render.
    stops: RefCell<HashMap<String, FocusHandle>>,
}

impl MachinesPage {
    pub(crate) fn new(cx: &mut App) -> Self {
        let destination = cx.new(|cx| TextInput::new("SSH destination", InputMode::SingleLine, cx));
        let _ = destination.read(cx).focus_handle(cx).tab_stop(true);
        Self {
            destination,
            link: cx.focus_handle().tab_stop(true),
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

impl Keystone {
    /// Opens Settings → Machines with the destination field focused, as Link a machine
    /// does.
    pub(crate) fn open_link_machine(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_machines(window, cx);
        if let Some(page) = &self.settings {
            page.machines.destination.focus_handle(cx).focus(window, cx);
        }
    }

    /// The Machines section's content, below its title.
    pub(crate) fn machines_section(
        &self,
        page: &SettingsPage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = self.state.read(cx);
        let workspace = state.store().workspace();
        let rows: Vec<(Machine, usize, LinkStatus, Option<String>)> = workspace
            .machines()
            .iter()
            .map(|machine| {
                let sessions = workspace
                    .sessions()
                    .iter()
                    .filter(|s| s.machine() == Some(machine.id()))
                    .count();
                let status = state.machine_status(machine.id());
                let detail = match &status {
                    LinkStatus::Failed(reason) => Some(reason.clone()),
                    _ => state.machine_welcome(machine.id()).map(|welcome| {
                        let claude = welcome
                            .providers
                            .iter()
                            .find(|found| found.provider == Provider::Claude)
                            .map_or_else(
                                || "Claude not found".to_owned(),
                                |found| match (&found.version, &found.missing) {
                                    (Some(version), _) => format!("Claude {version}"),
                                    (None, Some(missing)) => format!("Claude {missing}"),
                                    _ => "Claude not found".to_owned(),
                                },
                            );
                        format!("{claude} · {} {}", welcome.os, welcome.arch)
                    }),
                };
                (machine.clone(), sessions, status, detail)
            })
            .collect();
        let rows: Vec<AnyElement> = rows
            .into_iter()
            .enumerate()
            .map(|(at, (machine, sessions, status, detail))| {
                self.machine_row(&page.machines, machine, sessions, status, detail, cx)
                    .when(at > 0, |row| row.hairline_t(cx))
                    .into_any_element()
            })
            .collect();
        v_flex()
            .automation_id("settings-machines")
            .when(rows.is_empty(), |d| {
                d.child(
                    Label::new(
                        "Run Claude on another machine you reach over SSH. Its sessions keep running when Keystone quits.",
                    )
                    .size(TextSize::Ui)
                    .color(Color::Muted),
                )
            })
            .children(rows)
            .child(self.link_form(&page.machines, cx))
            .into_any_element()
    }

    fn machine_row(
        &self,
        page: &MachinesPage,
        machine: Machine,
        sessions: usize,
        status: LinkStatus,
        detail: Option<String>,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let key = machine.id().to_string();
        let id = machine.id().clone();
        let (word, color) = state_word(&status);
        let failed = matches!(status, LinkStatus::Failed(_));
        let title = h_flex()
            .gap_2()
            .child(Label::new(machine.label().to_owned()).truncate())
            .when(sessions > 0, |d| {
                d.child(
                    Label::new(format!(
                        "{sessions} session{}",
                        if sessions == 1 { "" } else { "s" }
                    ))
                    .size(TextSize::Sm)
                    .color(Color::Faint),
                )
            });
        let state = h_flex()
            .gap(px(5.))
            .min_w_0()
            .items_start()
            .automation_id(format!("machine-state-{key}"))
            .child(
                div()
                    .flex_none()
                    .mt(px(6.))
                    .size(px(6.))
                    .rounded_full()
                    .bg(color.to_hsla(cx)),
            )
            .child(
                div()
                    .flex_none()
                    .child(Label::new(word).size(TextSize::Sm).color(Color::Muted)),
            )
            .children(detail.map(|detail| {
                let detail = Label::new(format!("· {detail}"))
                    .size(TextSize::Sm)
                    .color(Color::Faint);
                // A failure's reason says what to do, so it wraps rather than ending in "…".
                div()
                    .flex_1()
                    .min_w_0()
                    .child(if failed { detail } else { detail.truncate() })
            }));
        let mut actions = h_flex().flex_none().gap_1();
        if failed {
            let retry = id.clone();
            actions = actions.child(
                Button::new(format!("machine-retry-{key}"), "Retry")
                    .size(ButtonSize::Compact)
                    .style(ButtonStyle::Subtle)
                    .track_focus(&page.stop(&format!("retry-{key}"), cx))
                    .debug_selector(|| format!("machine-retry-{key}"))
                    .on_click(self.scoped_listener(cx, move |this, _, _, cx| {
                        this.state
                            .update(cx, |state, cx| state.retry_machine(&retry, cx));
                    })),
            );
        }
        let (start, remove) = (id.clone(), id.clone());
        actions = actions
            .child(
                IconButton::new(format!("machine-new-session-{key}"), IconName::Pencil)
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Compact)
                    .track_focus(&page.stop(&format!("new-{key}"), cx))
                    .debug_selector(|| format!("machine-new-session-{key}"))
                    .aria_label("New Claude session on this machine")
                    .tooltip(Tooltip::text("New Claude session here"))
                    .on_click(self.scoped_listener(cx, move |this, _, window, cx| {
                        this.open_remote_folder(&start, window, cx)
                    })),
            )
            .child(
                IconButton::new(format!("machine-remove-{key}"), IconName::Close)
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Compact)
                    .disabled(sessions > 0)
                    .track_focus(&page.stop(&format!("remove-{key}"), cx))
                    .debug_selector(|| format!("machine-remove-{key}"))
                    .aria_label("Remove machine")
                    .tooltip(Tooltip::text(if sessions > 0 {
                        "In use"
                    } else {
                        "Remove"
                    }))
                    .on_click(self.scoped_listener(cx, move |this, _, _, cx| {
                        this.error = this
                            .state
                            .update(cx, |state, cx| state.remove_machine(&remove, cx))
                            .err()
                            .map(|error| format!("Machine not removed. {error}"));
                        this.refresh(cx);
                    })),
            );
        let end = machine.end_on_quit();
        let toggle = id.clone();
        v_flex()
            .automation_id(format!("machine-row-{key}"))
            .gap(spacing::SM)
            .py(spacing::SM)
            .child(
                h_flex()
                    .gap(spacing::MD)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.))
                            .child(title)
                            .when(machine.destination() != machine.label(), |d| {
                                d.child(
                                    Label::new(machine.destination().to_owned())
                                        .size(TextSize::Sm)
                                        .color(Color::Muted)
                                        .mono()
                                        .truncate(),
                                )
                            })
                            .child(state),
                    )
                    .child(actions),
            )
            .child(
                Switch::new(format!("machine-end-on-quit-{key}"), end)
                    .label("End sessions on this machine when Keystone quits")
                    .on_change(self.scoped_listener(cx, move |this, on: &bool, _, cx| {
                        this.error = this
                            .state
                            .update(cx, |state, cx| {
                                state.set_machine_end_on_quit(&toggle, *on, cx)
                            })
                            .err()
                            .map(|error| format!("Not saved. {error}"));
                        this.refresh(cx);
                    })),
            )
    }

    fn link_form(&self, page: &MachinesPage, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let colors = cx.theme().colors;
        let linking = self.state.read(cx).linking.as_ref();
        let progress = linking.map(|linking| match &linking.failed {
            None => (
                format!("Connecting to {}…", linking.destination),
                Color::Faint,
            ),
            Some(reason) => (reason.clone(), Color::Danger),
        });
        let submit = self.scoped_listener(cx, |this, event: &KeyDownEvent, _, cx| {
            if event.keystroke.key == "enter" {
                this.link_machine(cx);
                cx.stop_propagation();
            }
        });
        v_flex()
            .pt(spacing::LG)
            .gap(spacing::SM)
            .child(
                Label::new("Link a machine")
                    .size(TextSize::Ui)
                    .weight(FontWeight::MEDIUM)
                    .color(Color::Muted),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap(spacing::SM)
                    .on_key_down(submit)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(160.))
                            .px(px(8.))
                            .rounded(px(6.))
                            .border_1()
                            .border_color(colors.border)
                            .bg(colors.background)
                            .automation_id("machine-destination")
                            .child(page.destination.clone()),
                    )
                    .child(
                        div().flex_none().child(
                            Button::new("machine-link", "Link")
                                .style(ButtonStyle::Filled)
                                .track_focus(&page.link)
                                .debug_selector(|| "machine-link".into())
                                .on_click(self.scoped_listener(cx, |this, _, _, cx| {
                                    this.link_machine(cx)
                                })),
                        ),
                    ),
            )
            .children(progress.map(|(text, color)| {
                h_flex()
                    .gap(px(6.))
                    .items_start()
                    .automation_id("machine-link-state")
                    .child(
                        div()
                            .flex_none()
                            .mt(px(6.))
                            .size(px(6.))
                            .rounded_full()
                            .bg(color.to_hsla(cx)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(text).size(TextSize::Sm).color(Color::Muted)),
                    )
            }))
            .child(
                Label::new(
                    "A Host from ~/.ssh/config, user@host, or a tailnet name. Keystone signs in with your SSH keys and runs keystone-host there.",
                )
                .size(TextSize::Sm)
                .color(Color::Faint),
            )
    }

    fn link_machine(&mut self, cx: &mut Context<Self>) {
        let Some(page) = &self.settings else { return };
        let destination = page.machines.destination.read(cx).value().to_owned();
        self.state
            .update(cx, |state, cx| state.link_machine(&destination, cx));
        self.refresh(cx);
    }

    /// Clears the destination field once the machine it names is saved. A failed link
    /// keeps the text, so a typo can be fixed.
    pub(crate) fn clear_linked_destination(&mut self, cx: &mut Context<Self>) {
        let Some(page) = &self.settings else { return };
        let input = page.machines.destination.clone();
        let typed = input.read(cx).value().trim().to_owned();
        let state = self.state.read(cx);
        let saved = state.linking.is_none()
            && state
                .store()
                .workspace()
                .machines()
                .iter()
                .any(|machine| machine.destination() == typed);
        if saved {
            input.update(cx, |input, cx| input.set_value(String::new(), cx));
        }
    }

    /// Lists the linked machines to start a session on, and Link a machine.
    pub(crate) fn open_machine_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_palette(window, cx);
        self.close_menus();
        let state = self.state.read(cx);
        let mut items: Vec<PaletteItem> = state
            .store()
            .workspace()
            .machines()
            .iter()
            .map(|machine| {
                let (word, _) = state_word(&state.machine_status(machine.id()));
                PaletteItem::new(
                    format!("{MACHINE_ROW}{}", machine.id()),
                    machine.label().to_owned(),
                )
                .detail(word)
            })
            .collect();
        items.push(PaletteItem::new(LINK_ROW, "Link a machine…"));
        self.open_palette(items, crate::commands::Listing::Machines, window, cx);
    }

    /// Asks for the folder on `machine` a new Claude session starts in.
    pub(crate) fn open_remote_folder(
        &mut self,
        machine: &MachineId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_palette(window, cx);
        self.close_menus();
        let Some(label) = self
            .state
            .read(cx)
            .store()
            .workspace()
            .machine(machine)
            .map(|m| m.label().to_owned())
        else {
            return;
        };
        self.open_palette(
            Vec::new(),
            crate::commands::Listing::RemoteFolder(machine.clone()),
            window,
            cx,
        );
        if let Some(palette) = &self.palette {
            palette.state.update(cx, |state, cx| {
                state.set_query_item(
                    move |query| {
                        let folder = RemoteFolder::new(query).ok()?;
                        Some(
                            PaletteItem::new(
                                format!("{FOLDER_ROW}{}", folder.wire()),
                                format!("Start Claude in {}", folder.wire()),
                            )
                            .detail(label.clone()),
                        )
                    },
                    cx,
                );
            });
        }
    }

    pub(crate) fn confirm_machine(
        &mut self,
        item: &PaletteItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if item.id.as_ref() == LINK_ROW {
            self.close_palette(window, cx);
            return self.open_link_machine(window, cx);
        }
        let Some(id) = item.id.strip_prefix(MACHINE_ROW) else {
            return;
        };
        self.open_remote_folder(&MachineId::new(id), window, cx);
    }

    pub(crate) fn confirm_remote_folder(
        &mut self,
        machine: &MachineId,
        item: &PaletteItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(folder) = item
            .id
            .strip_prefix(FOLDER_ROW)
            .and_then(|path| RemoteFolder::new(path).ok())
        else {
            return;
        };
        self.close_palette(window, cx);
        if self.settings.is_some() {
            self.close_settings(window, cx);
        }
        let window_id: WindowId = self.window_id;
        let machine = machine.clone();
        self.create_with(cx, move |state, cx| {
            state.create_remote(&machine, &folder, window_id, None, cx)
        });
        if self.error.is_none() && self.selected(cx).is_some() {
            self.composer.focus_handle(cx).focus(window, cx);
        }
    }

    fn close_menus(&mut self) {
        self.new_menu = false;
        self.filter_menu = false;
        self.approval_menu = false;
        self.attach.menu = false;
        self.model_menu = None;
    }
}
