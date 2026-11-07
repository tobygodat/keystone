//! The settings page: it takes the place of the workspace body and edits the saved
//! appearance, one section at a time.

use crate::customization::OpenCustomize;
use crate::notifications::Alert;
use crate::settings::{self, ACTION_COLORS, DEFAULT_TEXT_SIZE, TEXT_SIZES, TRANSCRIPT_SIZES};
use crate::{
    FocusComposer, FocusSearch, Keystone, NewClaude, NewCodex, RenameSession, SendMessage,
    ToggleSettle, ToggleSidebar,
};
use gpui::{
    AnyElement, App, Context, FocusHandle, FontWeight, KeyDownEvent, Window, div, prelude::*, px,
};
use keystone_core::{Appearance, Density, Notifications, SidebarWidth, Sounds, ThemeMode};
use keystone_ui::prelude::*;

const THEMES: [(ThemeMode, &str); 3] = [
    (ThemeMode::System, "System"),
    (ThemeMode::Light, "Light"),
    (ThemeMode::Dark, "Dark"),
];

const DENSITIES: [(Density, &str, &str); 3] = [
    (Density::Compact, "Compact", "density-compact"),
    (Density::Default, "Default", "density-default"),
    (Density::Spacious, "Spacious", "density-spacious"),
];

#[derive(Clone, Copy)]
enum Field {
    Theme,
    UiFont,
    MonoFont,
    ActionColor,
    Density,
    TextSize,
    TranscriptSize,
    Notifications,
    TurnFinished,
    Approval,
    TurnFailed,
    PullRequests,
    Sounds,
    Volume,
    Sound(Alert),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Appearance,
    Fonts,
    Notifications,
    Sounds,
    Accounts,
    Integrations,
    Prompts,
    Scheduled,
    Project,
}

/// A section's rows, each a label and its control.
type Rows = &'static [(&'static str, Field)];

/// Each section with its label, automation name, and rows, in sidebar order.
const SECTIONS: [(Section, &str, &str, Rows); 9] = [
    (
        Section::Appearance,
        "Appearance",
        "settings-section-appearance",
        &[
            ("Theme", Field::Theme),
            ("Action color", Field::ActionColor),
            ("Density", Field::Density),
            ("Text size", Field::TextSize),
            ("Transcript text", Field::TranscriptSize),
        ],
    ),
    (
        Section::Fonts,
        "Fonts",
        "settings-section-fonts",
        &[
            ("Interface font", Field::UiFont),
            ("Code font", Field::MonoFont),
        ],
    ),
    (
        Section::Notifications,
        "Notifications",
        "settings-section-notifications",
        &[
            ("Desktop notifications", Field::Notifications),
            ("Turn finished", Field::TurnFinished),
            ("Approval requested", Field::Approval),
            ("Turn failed", Field::TurnFailed),
            ("Pull request checks and reviews", Field::PullRequests),
        ],
    ),
    (
        Section::Sounds,
        "Sounds",
        "settings-section-sounds",
        &[
            ("Play sounds", Field::Sounds),
            ("Volume", Field::Volume),
            ("Turn finished", Field::Sound(Alert::TurnFinished)),
            ("Approval requested", Field::Sound(Alert::Approval)),
            ("Turn failed", Field::Sound(Alert::TurnFailed)),
        ],
    ),
    (
        Section::Accounts,
        "Accounts",
        "settings-section-accounts",
        &[],
    ),
    (
        Section::Integrations,
        "Integrations",
        "settings-section-integrations",
        &[],
    ),
    (Section::Prompts, "Prompts", "settings-section-prompts", &[]),
    (
        Section::Scheduled,
        "Scheduled",
        "settings-section-scheduled",
        &[],
    ),
    (Section::Project, "Project", "settings-section-project", &[]),
];

/// The open settings page: the section it shows and a focus handle per tab stop. Only
/// the shown section's controls render, so only theirs are tab stops.
pub(crate) struct SettingsPage {
    section: Section,
    back: FocusHandle,
    sections: [FocusHandle; SECTIONS.len()],
    theme: FocusHandle,
    ui_font: FocusHandle,
    mono_font: FocusHandle,
    action_color: [FocusHandle; ACTION_COLORS.len()],
    density: [FocusHandle; DENSITIES.len()],
    text_size: FocusHandle,
    transcript_size: FocusHandle,
    pub(crate) accounts: crate::settings_accounts::AccountsPage,
    pub(crate) integrations: crate::integrations::IntegrationsPage,
    pub(crate) prompts: crate::settings_prompts::PromptsPage,
    pub(crate) scheduled: crate::settings_scheduled::ScheduledPage,
    pub(crate) project: crate::settings_project::ProjectPage,
}

impl SettingsPage {
    fn new(cx: &mut App) -> Self {
        let stop = || cx.focus_handle().tab_stop(true);
        Self {
            section: Section::Appearance,
            back: stop(),
            sections: std::array::from_fn(|_| stop()),
            theme: stop(),
            ui_font: stop(),
            mono_font: stop(),
            action_color: std::array::from_fn(|_| stop()),
            density: std::array::from_fn(|_| stop()),
            text_size: stop(),
            transcript_size: stop(),
            accounts: crate::settings_accounts::AccountsPage::new(cx),
            integrations: crate::integrations::IntegrationsPage::new(cx),
            prompts: crate::settings_prompts::PromptsPage::new(cx),
            scheduled: crate::settings_scheduled::ScheduledPage::new(cx),
            project: crate::settings_project::ProjectPage::new(cx),
        }
    }

    fn section_index(&self) -> usize {
        SECTIONS
            .iter()
            .position(|(section, ..)| *section == self.section)
            .unwrap()
    }
}

impl Keystone {
    /// Opens the page, or closes it when it is open. Opening closes any open menu and
    /// the customize panel, and focuses the selected section.
    pub(crate) fn toggle_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.is_some() {
            return self.close_settings(window, cx);
        }
        if self.presentation.current.customization.is_open() {
            self.customize(crate::arrangement_editor::Action::Close, window, cx);
        }
        self.new_menu = false;
        self.filter_menu = false;
        self.approval_menu = false;
        self.attach.menu = false;
        self.model_menu = None;
        self.presentation.current.arrangement.surface_menu = None;
        self.context_menu = None;
        let page = SettingsPage::new(cx);
        window.focus(&page.sections[page.section_index()], cx);
        self.settings = Some(page);
        self.refresh(cx);
    }

    /// Opens the page on Accounts, as the usage meter does.
    pub(crate) fn open_accounts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_section(Section::Accounts, window, cx)
    }

    /// Opens the page on Prompts, the prompt library.
    pub(crate) fn open_prompts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_section(Section::Prompts, window, cx)
    }

    /// Opens the page on Project, the window's project's own settings.
    pub(crate) fn open_project_section(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_section(Section::Project, window, cx)
    }

    fn open_section(&mut self, section: Section, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.is_none() {
            self.toggle_settings(window, cx);
        }
        if let Some(page) = &mut self.settings {
            page.section = section;
            window.focus(&page.sections[page.section_index()], cx);
        }
        if section == Section::Project {
            self.load_project_settings(cx);
        }
        self.refresh(cx);
    }

    fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings = None;
        self.focus_composer(window, cx);
        self.refresh(cx);
    }

    /// The page hides the workspace, so an action aimed at it closes the page before
    /// its handler runs and focuses the target. Send has nothing visible to send.
    pub(crate) fn settings_yields_to_workspace<E: InteractiveElement>(
        &self,
        root: E,
        cx: &Context<Self>,
    ) -> E {
        root.capture_action(self.leave_settings::<NewClaude>(cx))
            .capture_action(self.leave_settings::<NewCodex>(cx))
            .capture_action(self.leave_settings::<FocusSearch>(cx))
            .capture_action(self.leave_settings::<FocusComposer>(cx))
            .capture_action(self.leave_settings::<crate::transcript_keyboard::FocusTranscript>(cx))
            .capture_action(self.leave_settings::<crate::transcript_keyboard::NextTranscript>(cx))
            .capture_action(
                self.leave_settings::<crate::transcript_keyboard::PreviousTranscript>(cx),
            )
            .capture_action(self.leave_settings::<RenameSession>(cx))
            .capture_action(self.leave_settings::<ToggleSettle>(cx))
            .capture_action(self.leave_settings::<ToggleSidebar>(cx))
            .capture_action(self.leave_settings::<crate::files::ToggleFiles>(cx))
            .capture_action(self.leave_settings::<crate::notes::ToggleNotes>(cx))
            .capture_action(self.leave_settings::<crate::pull_request::TogglePullRequest>(cx))
            .capture_action(self.leave_settings::<crate::changes::ToggleChanges>(cx))
            .capture_action(self.leave_settings::<OpenCustomize>(cx))
            .capture_action(self.leave_settings::<crate::session_tabs::NewTab>(cx))
            .capture_action(self.leave_settings::<crate::session_tabs::CloseTab>(cx))
            .capture_action(self.leave_settings::<crate::session_tabs::NextTab>(cx))
            .capture_action(self.leave_settings::<crate::session_tabs::PreviousTab>(cx))
            .capture_action(self.leave_settings::<crate::session_tabs::SplitRight>(cx))
            .capture_action(self.leave_settings::<crate::prompts::InsertPrompt>(cx))
            .capture_action(self.scoped_listener(cx, |this, _: &SendMessage, _, cx| {
                if this.settings.is_some() {
                    cx.stop_propagation();
                }
            }))
    }

    /// Focus moves to the root, not the composer, so a handler that focuses nothing
    /// leaves the window's shortcuts working and one that does focus its target wins.
    fn leave_settings<A: 'static>(
        &self,
        cx: &Context<Self>,
    ) -> impl Fn(&A, &mut Window, &mut App) + 'static {
        self.scoped_listener(cx, |this, _: &A, window, cx| {
            if this.settings.take().is_some() {
                this.focus.focus(window, cx);
                this.refresh(cx);
            }
        })
    }

    fn update_notifications(
        &mut self,
        change: impl FnOnce(&mut Notifications),
        cx: &mut Context<Self>,
    ) {
        let mut notifications = self
            .state
            .read(cx)
            .store()
            .workspace()
            .preferences()
            .notifications();
        change(&mut notifications);
        self.error = match self
            .state
            .update(cx, |state, cx| state.set_notifications(notifications, cx))
        {
            Ok(_) => None,
            Err(error) => Some(format!("Settings not saved. {error}")),
        };
        self.refresh(cx);
    }

    /// Saves the sound settings `change` makes, then plays `preview` at the saved volume
    /// so the user hears what they chose. Without one, a preview still waiting is
    /// dropped.
    fn update_sounds(
        &mut self,
        change: impl FnOnce(&mut Sounds),
        preview: Option<Alert>,
        cx: &mut Context<Self>,
    ) {
        let mut sounds = self
            .state
            .read(cx)
            .store()
            .workspace()
            .preferences()
            .sounds();
        change(&mut sounds);
        self.error = match self
            .state
            .update(cx, |state, cx| state.set_sounds(sounds, cx))
        {
            Ok(_) => None,
            Err(error) => Some(format!("Settings not saved. {error}")),
        };
        let preview = preview.filter(|_| self.error.is_none());
        crate::sounds::SoundPlayer::preview(preview, sounds.volume, cx);
        self.refresh(cx);
    }

    fn show_section(&mut self, at: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(page) = &mut self.settings else {
            return;
        };
        let opened = page.section != SECTIONS[at].0;
        page.section = SECTIONS[at].0;
        window.focus(&page.sections[at], cx);
        let section = page.section;
        if opened && section == Section::Integrations {
            self.read_integrations(cx);
        }
        if opened && section == Section::Project {
            self.load_project_settings(cx);
        }
        cx.notify();
    }

    /// A handler that applies `edit` to the saved appearance.
    fn change<E: 'static>(
        &self,
        cx: &Context<Self>,
        edit: impl Fn(&mut Appearance, &E) + 'static,
    ) -> impl Fn(&E, &mut Window, &mut App) + 'static {
        self.scoped_listener(cx, move |this, event: &E, window, cx| {
            this.update_appearance(|appearance| edit(appearance, event), window, cx)
        })
    }

    /// The page in place of the workspace body, or `None` while it is closed.
    pub(crate) fn settings_page(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let page = self.settings.as_ref()?;
        let at = page.section_index();
        let (_, title, _, rows) = SECTIONS[at];
        let appearance = self
            .state
            .read(cx)
            .store()
            .workspace()
            .preferences()
            .appearance();
        let audible = crate::sounds::SoundPlayer::available(cx);
        let rows = rows
            .iter()
            .filter(|(_, field)| audible || !matches!(field, Field::Volume | Field::Sound(_)))
            .enumerate()
            .map(|(row, &(label, field))| {
                h_flex()
                    .gap(spacing::MD)
                    .py(spacing::MD)
                    .when(row > 0, |d| d.hairline_t(cx))
                    .child(div().flex_1().min_w_0().child(Label::new(label).truncate()))
                    .child(self.control(page, field, appearance, cx))
            })
            .collect::<Vec<_>>();
        let custom = match page.section {
            Section::Accounts => Some(self.accounts_section(page, cx).into_any_element()),
            Section::Integrations => Some(self.integrations_section(page, cx).into_any_element()),
            Section::Prompts => Some(self.prompts_section(page, cx).into_any_element()),
            Section::Scheduled => Some(self.scheduled_section(page, cx).into_any_element()),
            Section::Project => Some(self.project_section(page, cx)),
            _ => None,
        };
        let content = div()
            .id("settings-content")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(640.))
                    .mx_auto()
                    .px(spacing::LG)
                    .py(spacing::XL)
                    .child(
                        div().pb(spacing::LG).child(
                            Label::new(title)
                                .size(TextSize::Title)
                                .weight(FontWeight::SEMIBOLD)
                                .color(Color::Strong),
                        ),
                    )
                    .when(!rows.is_empty(), |d| {
                        d.child(
                            v_flex()
                                .surface(Elevation::Surface, cx)
                                .px(spacing::LG)
                                .children(rows),
                        )
                    })
                    .children(custom.map(|section| {
                        v_flex()
                            .surface(Elevation::Surface, cx)
                            .p(spacing::LG)
                            .child(section)
                    }))
                    .children(self.error_line(cx).map(|error| {
                        div()
                            .pt(spacing::MD)
                            .automation_id("settings-error")
                            .child(error)
                    })),
            );
        Some(
            h_flex()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(self.settings_column(page, cx))
                .child(content)
                .automation_id("settings-page")
                .into_any_element(),
        )
    }

    /// Back, the sections, and the sidebar's own footer, so its cog stays put.
    fn settings_column(&self, page: &SettingsPage, cx: &mut Context<Self>) -> impl IntoElement {
        let back = ListItem::new("settings-back")
            .track_focus(&page.back)
            .debug_selector(|| "settings-back".into())
            .on_click(
                self.scoped_listener(cx, |this, _, window, cx| this.close_settings(window, cx)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(Icon::new(IconName::Left).size(px(14.)).color(Color::Muted))
                    .child(Label::new("Back").color(Color::Muted)),
            );
        let selected = page.section_index();
        let sections = SECTIONS.iter().zip(&page.sections).enumerate().map(
            |(at, (&(_, label, id, _), focus))| {
                let title = Label::new(label);
                ListItem::new(id)
                    .track_focus(focus)
                    .debug_selector(|| id.into())
                    .selected(at == selected)
                    .on_click(self.scoped_listener(cx, move |this, _, window, cx| {
                        this.show_section(at, window, cx)
                    }))
                    .child(if at == selected {
                        title.weight(FontWeight::MEDIUM).color(Color::Strong)
                    } else {
                        title
                    })
            },
        );
        v_flex()
            .w(px(f32::from(SidebarWidth::DEFAULT.px())))
            .flex_shrink_0()
            .h_full()
            .bg(cx.theme().colors.sidebar)
            .hairline_r(cx)
            .child(
                v_flex()
                    .id("settings-sections")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_2()
                    .py(px(6.))
                    .gap(px(2.))
                    .child(back)
                    .children(sections),
            )
            .child(self.sidebar_footer(cx))
    }

    fn font_select(
        &self,
        id: &'static str,
        focus: &FocusHandle,
        choices: Vec<String>,
        current: Option<&String>,
        set: fn(&mut Appearance, Option<String>),
        cx: &Context<Self>,
    ) -> AnyElement {
        let selected = current
            .and_then(|font| choices.iter().position(|choice| choice == font))
            .map_or(0, |at| at + 1);
        let options: Vec<String> = std::iter::once("Default".to_owned())
            .chain(choices.iter().cloned())
            .collect();
        Select::new(id, options, Some(selected))
            .track_focus(focus)
            .on_change(self.change(cx, move |appearance, &at: &usize| {
                set(appearance, at.checked_sub(1).map(|at| choices[at].clone()))
            }))
            .into_any_element()
    }

    fn control(
        &self,
        page: &SettingsPage,
        field: Field,
        saved: &Appearance,
        cx: &Context<Self>,
    ) -> AnyElement {
        match field {
            Field::Theme => Select::new(
                "settings-theme",
                THEMES.map(|(_, label)| label),
                THEMES.iter().position(|(mode, _)| *mode == saved.theme),
            )
            .track_focus(&page.theme)
            .on_change(self.change(cx, |appearance, &at: &usize| {
                appearance.theme = THEMES[at].0
            }))
            .into_any_element(),
            Field::UiFont => self.font_select(
                "settings-ui-font",
                &page.ui_font,
                settings::font_choices(saved, cx),
                saved.ui_font.as_ref(),
                |appearance, font| appearance.ui_font = font,
                cx,
            ),
            Field::MonoFont => self.font_select(
                "settings-mono-font",
                &page.mono_font,
                settings::mono_choices(saved, cx),
                saved.mono_font.as_ref(),
                |appearance, font| appearance.mono_font = font,
                cx,
            ),
            Field::ActionColor => h_flex()
                .flex_shrink_0()
                .gap_1()
                .children(
                    ACTION_COLORS
                        .iter()
                        .zip(&page.action_color)
                        .enumerate()
                        .map(|(n, (&(saved_color, shown), focus))| {
                            let id = format!("settings-action-color-{n}");
                            Swatch::new(
                                gpui::SharedString::from(id.clone()),
                                shown.tint(cx.theme().appearance).fill,
                            )
                            .selected(saved.action_color == saved_color)
                            .track_focus(focus)
                            .tooltip(Tooltip::text(shown.name()))
                            .aria_label(shown.name())
                            .debug_selector(|| id)
                            .on_click(
                                self.change(cx, move |appearance, _| {
                                    appearance.action_color = saved_color
                                }),
                            )
                        }),
                )
                .into_any_element(),
            Field::Density => h_flex()
                .flex_shrink_0()
                .gap_1()
                .children(DENSITIES.iter().zip(&page.density).map(
                    |(&(density, label, id), focus)| {
                        Button::new(id, label)
                            .style(ButtonStyle::Subtle)
                            .selected(saved.density == density)
                            .track_focus(focus)
                            .debug_selector(|| id.into())
                            .on_click(
                                self.change(cx, move |appearance, _| appearance.density = density),
                            )
                    },
                ))
                .into_any_element(),
            Field::Notifications
            | Field::TurnFinished
            | Field::Approval
            | Field::TurnFailed
            | Field::PullRequests => {
                let saved = self
                    .state
                    .read(cx)
                    .store()
                    .workspace()
                    .preferences()
                    .notifications();
                let (id, on, set): (_, _, fn(&mut Notifications, bool)) = match field {
                    Field::Notifications => ("settings-notifications", saved.enabled, |n, on| {
                        n.enabled = on
                    }),
                    Field::TurnFinished => (
                        "settings-notify-turn-finished",
                        saved.turn_finished,
                        |n, on| n.turn_finished = on,
                    ),
                    Field::Approval => ("settings-notify-approval", saved.approval, |n, on| {
                        n.approval = on
                    }),
                    Field::PullRequests => (
                        "settings-notify-pull-requests",
                        saved.pull_requests,
                        |n, on| n.pull_requests = on,
                    ),
                    _ => ("settings-notify-turn-failed", saved.turn_failed, |n, on| {
                        n.turn_failed = on
                    }),
                };
                Switch::new(id, on)
                    .disabled(!matches!(field, Field::Notifications) && !saved.enabled)
                    .on_change(self.scoped_listener(cx, move |this, &on: &bool, _, cx| {
                        this.update_notifications(|n| set(n, on), cx)
                    }))
                    .into_any_element()
            }
            Field::Sounds | Field::Volume | Field::Sound(_) => self.sound_control(field, cx),
            Field::TranscriptSize => {
                let size = saved
                    .transcript_size
                    .unwrap_or_else(|| f32::from(cx.theme().transcript_text().0) as u8);
                h_flex()
                    .w(px(220.))
                    .gap_2()
                    .child(
                        div().flex_1().child(
                            Slider::new(
                                "settings-transcript-size",
                                f32::from(size),
                                f32::from(*TRANSCRIPT_SIZES.start())
                                    ..=f32::from(*TRANSCRIPT_SIZES.end()),
                                1.,
                            )
                            .track_focus(&page.transcript_size)
                            .on_change(self.change(
                                cx,
                                |appearance, &size: &f32| {
                                    appearance.transcript_size = Some(size as u8)
                                },
                            )),
                        ),
                    )
                    .child(
                        Label::new(format!("{size} px"))
                            .size(TextSize::Ui)
                            .color(Color::Muted),
                    )
                    .into_any_element()
            }
            Field::TextSize => {
                let size = saved.text_size.unwrap_or(DEFAULT_TEXT_SIZE);
                h_flex()
                    .w(px(220.))
                    .gap_2()
                    .child(
                        div().flex_1().child(
                            Slider::new(
                                "settings-text-size",
                                f32::from(size),
                                f32::from(*TEXT_SIZES.start())..=f32::from(*TEXT_SIZES.end()),
                                1.,
                            )
                            .track_focus(&page.text_size)
                            .on_change(
                                self.change(cx, |appearance, &size: &f32| {
                                    appearance.text_size = Some(size as u8)
                                }),
                            ),
                        ),
                    )
                    .child(
                        Label::new(format!("{size} px"))
                            .size(TextSize::Ui)
                            .color(Color::Muted),
                    )
                    .into_any_element()
            }
        }
    }

    /// A sound setting's control. Without a player, the global row says what to install
    /// in place of its switch, and the other rows are hidden.
    fn sound_control(&self, field: Field, cx: &Context<Self>) -> AnyElement {
        let saved = self
            .state
            .read(cx)
            .store()
            .workspace()
            .preferences()
            .sounds();
        let available = crate::sounds::SoundPlayer::available(cx);
        let first_on = [Alert::TurnFinished, Alert::Approval, Alert::TurnFailed]
            .into_iter()
            .find(|alert| {
                alert.sounds(Sounds {
                    enabled: true,
                    ..saved
                })
            });
        let (id, on, set, preview): (_, _, fn(&mut Sounds, bool), _) = match field {
            Field::Sounds if !available => {
                return Label::new(if cfg!(target_os = "macos") {
                    "Needs afplay"
                } else {
                    "Needs paplay or pw-play"
                })
                .color(Color::Muted)
                .into_any_element();
            }
            Field::Sounds => (
                "settings-sounds",
                saved.enabled,
                |s, on| s.enabled = on,
                first_on,
            ),
            Field::Volume => {
                return div()
                    .w(px(220.))
                    .child(
                        Slider::new(
                            "settings-sound-volume",
                            f32::from(saved.volume),
                            0.0..=100.,
                            10.,
                        )
                        .disabled(!saved.enabled)
                        .on_change(self.scoped_listener(
                            cx,
                            move |this, &volume: &f32, _, cx| {
                                this.update_sounds(|s| s.volume = volume as u8, first_on, cx)
                            },
                        )),
                    )
                    .into_any_element();
            }
            Field::Sound(Alert::TurnFinished) => (
                "settings-sound-turn-finished",
                saved.turn_finished,
                |s, on| s.turn_finished = on,
                Some(Alert::TurnFinished),
            ),
            Field::Sound(Alert::Approval) => (
                "settings-sound-approval",
                saved.approval,
                |s, on| s.approval = on,
                Some(Alert::Approval),
            ),
            _ => (
                "settings-sound-turn-failed",
                saved.turn_failed,
                |s, on| s.turn_failed = on,
                Some(Alert::TurnFailed),
            ),
        };
        Switch::new(id, on)
            .disabled(!matches!(field, Field::Sounds) && !saved.enabled)
            .on_change(self.scoped_listener(cx, move |this, &on: &bool, _, cx| {
                this.update_sounds(|s| set(s, on), preview.filter(|_| on), cx)
            }))
            .into_any_element()
    }

    /// Escape closes the page. Up and Down on a focused section move the selection, and
    /// Left and Right on a focused density button move the choice. An open select menu
    /// handles its own keys first.
    pub(crate) fn page_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(page) = &self.settings else {
            return false;
        };
        let keystroke = &event.keystroke;
        if keystroke.modifiers.modified() {
            return false;
        }
        let focused = |stops: &[FocusHandle]| stops.iter().position(|stop| stop.is_focused(window));
        match keystroke.key.as_str() {
            "escape" => self.close_settings(window, cx),
            "up" | "down" => {
                let Some(at) = focused(&page.sections) else {
                    return false;
                };
                let next = if keystroke.key == "down" {
                    (at + 1).min(SECTIONS.len() - 1)
                } else {
                    at.saturating_sub(1)
                };
                self.show_section(next, window, cx);
            }
            "left" | "right" => {
                let step = |at: usize, len: usize| {
                    if keystroke.key == "right" {
                        (at + 1).min(len - 1)
                    } else {
                        at.saturating_sub(1)
                    }
                };
                let saved = self
                    .state
                    .read(cx)
                    .store()
                    .workspace()
                    .preferences()
                    .appearance()
                    .clone();
                if let Some(at) = focused(&page.density) {
                    let next = step(at, DENSITIES.len());
                    window.focus(&page.density[next], cx);
                    let density = DENSITIES[next].0;
                    if saved.density != density {
                        self.update_appearance(
                            |appearance| appearance.density = density,
                            window,
                            cx,
                        );
                    }
                } else if let Some(at) = focused(&page.action_color) {
                    let next = step(at, ACTION_COLORS.len());
                    window.focus(&page.action_color[next], cx);
                    let color = ACTION_COLORS[next].0;
                    if saved.action_color != color {
                        self.update_appearance(
                            |appearance| appearance.action_color = color,
                            window,
                            cx,
                        );
                    }
                } else {
                    return false;
                }
            }
            _ => return false,
        }
        true
    }
}

#[cfg(all(test, feature = "ui-tests"))]
mod tests {
    use crate::{Keystone, bind_keys};
    use gpui::{
        AppContext, Entity, Focusable, Modifiers, TestAppContext, VisualTestContext, point, px,
    };
    use keystone_core::{Appearance, Density, Notifications, Store, ThemeMode};
    use keystone_ui::{ActiveTheme, TextSize};
    use std::path::Path;

    const DRAFT: &str = "keep this draft";

    /// A workspace with one session holding a draft, and fonts saved that no machine
    /// has, so each font menu lists them after "Default".
    fn seeded(path: &Path) {
        std::fs::write(
            path,
            format!(
                r#"{{"version":1,"next_id":2,"workspace":{{"sessions":[{{"id":1,"title":"One","provider":"Codex","settled":false,"draft":"{DRAFT}"}}],"selected":1,"preferences":{{"ui_font":"Seed Sans","mono_font":"Seed Mono"}}}}}}"#
            ),
        )
        .unwrap();
    }

    fn open_window<'a>(
        path: &Path,
        cx: &'a mut TestAppContext,
    ) -> (Entity<Keystone>, &'a mut VisualTestContext) {
        let store = Store::open(path).unwrap();
        cx.update(|cx| {
            keystone_ui::init(cx);
            bind_keys(cx);
        });
        cx.executor().allow_parking();
        cx.add_window_view(|window, cx| {
            let view = Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            );
            view.focus.focus(window, cx);
            view
        })
    }

    fn shown(cx: &mut VisualTestContext, selector: &'static str) -> bool {
        cx.debug_bounds(selector).is_some()
    }

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} rendered"));
        cx.simulate_click(bounds.center(), Modifiers::none());
        cx.run_until_parked();
    }

    /// A key press and release. GPUI clicks a focused button on the release.
    fn press(cx: &mut VisualTestContext, key: &str) {
        cx.simulate_keystrokes(key);
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse(key).unwrap(),
        });
        cx.run_until_parked();
    }

    fn keys(cx: &mut VisualTestContext, keys: &str) {
        cx.simulate_keystrokes(keys);
        cx.run_until_parked();
    }

    fn saved(path: &Path) -> Appearance {
        Store::open(path)
            .unwrap()
            .workspace()
            .preferences()
            .appearance()
            .clone()
    }

    fn composer_focused(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> bool {
        cx.update(|window, cx| view.read(cx).composer.focus_handle(cx).is_focused(window))
    }

    fn height(cx: &mut VisualTestContext, selector: &'static str) -> gpui::Pixels {
        cx.debug_bounds(selector).unwrap().size.height
    }

    #[gpui::test]
    fn settings_escape_closes_after_clicking_blank_content(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        seeded(&path);
        let (view, cx) = open_window(&path, cx);
        keys(cx, "secondary-,");
        let track = cx.debug_bounds("settings-text-size").unwrap();
        cx.simulate_click(
            point(track.center().x, track.bottom() + px(50.)),
            Modifiers::none(),
        );
        cx.run_until_parked();
        keys(cx, "escape");
        assert!(
            !shown(cx, "settings-page"),
            "Escape closes from blank content"
        );
        assert!(composer_focused(&view, cx));
    }

    #[gpui::test]
    fn settings_failed_save_is_visible_and_retry_clears_it(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        seeded(&path);
        let (view, cx) = open_window(&path, cx);
        keys(cx, "secondary-,");
        let writer = crate::tests::lock_database(&path);
        click(cx, "settings-theme");
        click(cx, "settings-theme-option-1");
        click(cx, "settings-action-color-2");
        let track = cx.debug_bounds("settings-transcript-size").unwrap();
        cx.simulate_click(
            point(track.right() - px(1.), track.center().y),
            Modifiers::none(),
        );
        cx.run_until_parked();
        let state = view.read_with(cx, |view, _| view.state.clone());
        crate::tests::settle(&state, cx);
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            let appearance = view
                .state
                .read(cx)
                .store()
                .workspace()
                .preferences()
                .appearance();
            assert_eq!(appearance.action_color, keystone_core::ActionColor::Green);
            assert_eq!(appearance.transcript_size, Some(20));
            assert_eq!(cx.theme().transcript_text().0, px(20.));
            let tint = keystone_ui::ActionColor::Green.tint(cx.theme().appearance);
            assert_eq!(cx.theme().colors.element_inverse, tint.fill);
            assert!(view.status_line(cx).unwrap().0.starts_with("Not saved."));
            assert_eq!(
                view.state
                    .read(cx)
                    .store()
                    .workspace()
                    .preferences()
                    .appearance()
                    .theme,
                ThemeMode::Light
            );
        });
        assert!(shown(cx, "settings-error"), "the page shows the save error");
        drop(writer);
        click(cx, "settings-theme");
        click(cx, "settings-theme-option-1");
        let repaired = saved(&path);
        assert_eq!(repaired.theme, ThemeMode::Light);
        assert_eq!(repaired.action_color, keystone_core::ActionColor::Green);
        assert_eq!(repaired.transcript_size, Some(20));
        crate::tests::settle(&state, cx);
        cx.run_until_parked();
        assert!(
            !shown(cx, "settings-error"),
            "a successful save clears the error"
        );
    }

    fn accounts(path: &Path) -> Vec<(String, String, std::path::PathBuf)> {
        Store::open(path)
            .unwrap()
            .workspace()
            .accounts()
            .iter()
            .map(|a| {
                (
                    a.id().to_string(),
                    a.label().to_owned(),
                    a.home().to_owned(),
                )
            })
            .collect()
    }

    #[gpui::test]
    fn settings_accounts_add_copy_rename_and_remove_without_credentials(cx: &mut TestAppContext) {
        // Canonical, like the folders Keystone records: macOS's temporary folder sits
        // behind a symlink.
        let dir = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let path = dir.path().join("workspace.json");
        seeded(&path);
        let (view, cx) = open_window(&path, cx);
        keys(cx, "secondary-,");
        click(cx, "settings-section-accounts");
        assert!(shown(cx, "settings-accounts"));
        assert!(shown(cx, "account-row-claude-default"));
        assert!(shown(cx, "account-row-codex-default"));
        assert!(
            !shown(cx, "account-remove-claude-default"),
            "the default stays"
        );

        click(cx, "account-copy-claude-default");
        let clipboard = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()))
        };
        assert_eq!(
            clipboard(cx),
            Some(keystone_agent::default_sign_in_command(
                keystone_core::Provider::Claude
            ))
        );

        let folder = dir.path().join("claude work");
        click(cx, "account-name");
        cx.simulate_input("Work");
        click(cx, "account-folder");
        cx.simulate_input(folder.to_str().unwrap());
        click(cx, "account-add");
        assert_eq!(
            accounts(&path),
            [("claude-work".into(), "Work".into(), folder.clone())]
        );
        assert!(folder.is_dir());
        assert!(shown(cx, "account-row-claude-work"));
        click(cx, "account-copy-claude-work");
        assert_eq!(
            clipboard(cx),
            Some(format!(
                "CLAUDE_CONFIG_DIR='{}' claude auth login",
                folder.display()
            ))
        );

        click(cx, "account-rename-claude-work");
        keys(cx, "secondary-a");
        cx.simulate_input("Max two");
        keys(cx, "enter");
        assert_eq!(accounts(&path)[0].1, "Max two");

        click(cx, "account-name");
        cx.simulate_input("Again");
        click(cx, "account-folder");
        cx.simulate_input(folder.join("inner").to_str().unwrap());
        click(cx, "account-add");
        view.read_with(cx, |view, _| {
            let error = view.error.as_deref().unwrap();
            assert!(error.starts_with("Account not added."), "{error}");
        });
        assert!(shown(cx, "settings-error"));
        assert_eq!(accounts(&path).len(), 1);

        click(cx, "account-remove-claude-work");
        assert!(accounts(&path).is_empty());
        assert!(folder.is_dir(), "the folder stays on disk");
    }

    #[gpui::test]
    fn the_composer_offers_accounts_only_once_one_is_added_and_fixes_it_at_send(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        {
            let mut store = Store::open(&path).unwrap();
            let directory = keystone_core::WorkingDirectory::new(dir.path()).unwrap();
            store
                .create(keystone_core::Provider::Claude, &directory, 1)
                .unwrap();
        }
        let (view, cx) = open_window(&path, cx);
        click(cx, "model-selector");
        assert!(shown(cx, "provider-claude"));
        assert!(
            !shown(cx, "account-claude-default"),
            "one account is never named"
        );
        keys(cx, "escape");

        let home = dir.path().join("claude-work");
        view.update(cx, |view, cx| {
            view.state.update(cx, |state, cx| {
                state
                    .add_account(
                        keystone_core::Provider::Claude,
                        "Work",
                        home.to_str().unwrap(),
                        cx,
                    )
                    .unwrap();
            })
        });
        click(cx, "model-selector");
        assert!(shown(cx, "account-claude-default"));
        assert!(
            !shown(cx, "account-codex-default"),
            "Codex has no account added"
        );
        click(cx, "account-claude-work");
        let saved_account = || {
            Store::open(&path).unwrap().workspace().sessions()[0]
                .account()
                .map(|a| a.to_string())
        };
        assert_eq!(saved_account().as_deref(), Some("claude-work"));
        view.read_with(cx, |view, cx| {
            let selection = view.model_selection(1, keystone_core::Provider::Claude, cx);
            assert_eq!(selection.account.map(|a| a.as_str()), Some("claude-work"));
        });

        view.update(cx, |view, cx| {
            view.state
                .update(cx, |state, cx| state.lock_model(1, cx).unwrap());
        });
        view.update(cx, |view, cx| {
            let refused = view
                .state
                .update(cx, |state, cx| {
                    state.set_provider_account(1, keystone_core::Provider::Claude, None, cx)
                })
                .unwrap_err();
            assert!(refused.to_string().contains("fixed once"), "{refused}");
        });
        assert_eq!(saved_account().as_deref(), Some("claude-work"));
    }

    #[gpui::test]
    fn the_add_account_controls_never_overlap_at_any_width(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        seeded(&path);
        let (_view, cx) = open_window(&path, cx);
        for width in [1280., 900., 640., 480.] {
            cx.simulate_resize(gpui::size(px(width), px(900.)));
            keys(cx, "secondary-,");
            click(cx, "settings-section-accounts");
            let bounds = |cx: &mut VisualTestContext, id: &'static str| {
                cx.debug_bounds(id)
                    .unwrap_or_else(|| panic!("{id} not rendered at {width}px"))
            };
            let provider = bounds(cx, "account-provider");
            let name = bounds(cx, "account-name");
            let folder = bounds(cx, "account-folder");
            let add = bounds(cx, "account-add");
            let form = bounds(cx, "settings-accounts");
            let apart = |a: gpui::Bounds<gpui::Pixels>, b: gpui::Bounds<gpui::Pixels>| {
                a.right() <= b.origin.x
                    || b.right() <= a.origin.x
                    || a.bottom() <= b.origin.y
                    || b.bottom() <= a.origin.y
            };
            let controls = [provider, name, folder, add];
            for (i, a) in controls.iter().enumerate() {
                for b in &controls[i + 1..] {
                    assert!(apart(*a, *b), "{width}px: {a:?} overlaps {b:?}");
                }
            }
            for control in controls {
                assert!(
                    control.origin.x >= form.origin.x && control.right() <= form.right(),
                    "{width}px: {control:?} leaves {form:?}"
                );
            }
            assert!(
                name.size.width >= px(120.) && folder.size.width >= px(120.),
                "{width}px: name {name:?} folder {folder:?} form {form:?}"
            );
            keys(cx, "escape");
        }
    }

    #[gpui::test]
    fn a_long_account_list_scrolls_inside_the_menu(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        {
            let mut store = Store::open(&path).unwrap();
            let directory = keystone_core::WorkingDirectory::new(dir.path()).unwrap();
            for n in 0..30 {
                store
                    .add_account(
                        keystone_core::Provider::Claude,
                        &format!("Account {n}"),
                        dir.path().join(format!("claude-{n}")),
                        &[],
                    )
                    .unwrap();
            }
            store
                .create(keystone_core::Provider::Claude, &directory, 1)
                .unwrap();
        }
        let (_view, cx) = open_window(&path, cx);
        cx.simulate_resize(gpui::size(px(1280.), px(900.)));
        cx.run_until_parked();
        click(cx, "model-selector");
        let list = cx.debug_bounds("provider-list").unwrap();
        assert!(list.size.height <= px(360.), "{list:?}");
        assert!(
            list.origin.y >= px(0.) && list.bottom() <= px(900.),
            "the menu stays in the window: {list:?}"
        );
        let codex = cx.debug_bounds("provider-codex").unwrap();
        assert!(
            codex.origin.y > list.bottom(),
            "Codex starts below the fold"
        );
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: list.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-5000.))),
            ..Default::default()
        });
        cx.run_until_parked();
        let codex = cx.debug_bounds("provider-codex").unwrap();
        assert!(
            codex.origin.y >= list.origin.y && codex.bottom() <= list.bottom(),
            "scrolling reaches Codex: {codex:?} in {list:?}"
        );
    }

    #[gpui::test]
    fn the_sidebar_names_only_accounts_other_than_the_default(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        {
            let mut store = Store::open(&path).unwrap();
            let directory = keystone_core::WorkingDirectory::new(dir.path()).unwrap();
            let work = store
                .add_account(
                    keystone_core::Provider::Claude,
                    "Work",
                    dir.path().join("claude-work"),
                    &[],
                )
                .unwrap()
                .value;
            for account in [None, Some(work)] {
                let id = store
                    .create(keystone_core::Provider::Claude, &directory, 1)
                    .unwrap()
                    .value;
                store.set_account(id, account).unwrap();
            }
            let project = keystone_core::ProjectId::new(dir.path()).unwrap();
            store
                .open_project(keystone_core::WindowId::FIRST, &project, None, Some(2), 1)
                .unwrap();
        }
        let (_view, cx) = open_window(&path, cx);
        cx.run_until_parked();
        assert!(
            !shown(cx, "session-account-1"),
            "a default-account row looks as it did before accounts"
        );
        assert!(shown(cx, "session-account-2"));
    }

    #[gpui::test]
    fn notification_switches_save_and_the_global_one_holds_the_rest(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        seeded(&path);
        let (_view, cx) = open_window(&path, cx);
        let saved = |path: &Path| {
            Store::open(path)
                .unwrap()
                .workspace()
                .preferences()
                .notifications()
        };
        keys(cx, "secondary-,");
        click(cx, "settings-section-notifications");
        assert!(
            !shown(cx, "settings-theme"),
            "Notifications replaces Appearance"
        );
        assert_eq!(saved(&path), Notifications::default(), "all on by default");

        click(cx, "settings-notify-approval");
        assert!(!saved(&path).approval);
        click(cx, "settings-notifications");
        assert!(!saved(&path).enabled);
        click(cx, "settings-notify-turn-failed");
        assert!(
            saved(&path).turn_failed,
            "an event switch does nothing while all are off"
        );
        click(cx, "settings-notifications");
        click(cx, "settings-notify-turn-finished");
        assert_eq!(
            saved(&path),
            Notifications {
                turn_finished: false,
                approval: false,
                ..Notifications::default()
            }
        );
    }

    #[gpui::test]
    fn settings_changes_persist_preferences(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        seeded(&path);
        let (view, cx) = open_window(&path, cx);
        assert_eq!(height(cx, "settings"), px(28.));
        let row = height(cx, "session-1");
        assert!(!shown(cx, "settings-page"));

        keys(cx, "secondary-,");
        assert!(shown(cx, "settings-page"), "Ctrl+Comma opens");
        assert!(!shown(cx, "session-1"), "the page replaces the workspace");

        click(cx, "settings-theme");
        click(cx, "settings-theme-option-1");
        assert_eq!(saved(&path).theme, ThemeMode::Light);
        cx.update(|_, cx| assert_eq!(cx.theme().appearance, keystone_ui::Appearance::Light));

        click(cx, "density-compact");
        assert_eq!(saved(&path).density, Density::Compact);
        cx.update(|_, cx| assert_eq!(cx.theme().density, keystone_ui::Density::Compact));
        assert_eq!(height(cx, "settings"), px(24.));

        assert!(!shown(cx, "settings-ui-font"));
        click(cx, "settings-section-fonts");
        assert!(!shown(cx, "settings-theme"), "Fonts replaces Appearance");
        assert_eq!(saved(&path).ui_font.as_deref(), Some("Seed Sans"));
        click(cx, "settings-ui-font");
        click(cx, "settings-ui-font-option-0");
        assert_eq!(saved(&path).ui_font, None);

        assert_eq!(saved(&path).mono_font.as_deref(), Some("Seed Mono"));
        click(cx, "settings-mono-font");
        click(cx, "settings-mono-font-option-0");
        assert_eq!(saved(&path).mono_font, None);

        click(cx, "settings-mono-font");
        assert!(shown(cx, "settings-mono-font-option-0"));
        keys(cx, "escape");
        assert!(!shown(cx, "settings-mono-font-option-0"));
        assert!(
            shown(cx, "settings-page"),
            "Escape closes the open menu first"
        );
        keys(cx, "escape");
        assert!(!shown(cx, "settings-page"));
        assert!(composer_focused(&view, cx));
        assert!(
            height(cx, "session-1") < row,
            "compact density shrinks sidebar rows"
        );

        keys(cx, "secondary-,");
        let track = cx.debug_bounds("settings-text-size").unwrap();
        cx.simulate_click(
            point(track.right() - px(1.), track.center().y),
            Modifiers::none(),
        );
        cx.run_until_parked();
        assert_eq!(saved(&path).text_size, Some(16));
        cx.update(|_, _| assert_eq!(TextSize::Base.px(), px(16.)));

        keys(cx, "escape");
        view.read_with(cx, |view, cx| {
            assert_eq!(view.composer.read(cx).value(), DRAFT)
        });
        assert_eq!(
            Store::open(&path).unwrap().workspace().sessions()[0].draft(),
            DRAFT
        );
    }

    #[gpui::test]
    fn action_color_and_transcript_text_apply_and_persist(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        seeded(&path);
        let (_view, cx) = open_window(&path, cx);
        keys(cx, "secondary-,");

        click(cx, "settings-action-color-2");
        assert_eq!(saved(&path).action_color, keystone_core::ActionColor::Green);
        cx.update(|_, cx| {
            let theme = cx.theme();
            let green = keystone_ui::ActionColor::Green.tint(theme.appearance);
            assert_eq!(theme.colors.element_inverse, green.fill);
            assert_eq!(theme.colors.text_inverse, green.text);
        });
        click(cx, "settings-action-color-0");
        assert_eq!(saved(&path).action_color, keystone_core::ActionColor::Ink);

        cx.update(|_, cx| assert_eq!(cx.theme().transcript_text(), (px(14.), px(22.))));
        let track = cx.debug_bounds("settings-transcript-size").unwrap();
        cx.simulate_click(
            point(track.right() - px(1.), track.center().y),
            Modifiers::none(),
        );
        cx.run_until_parked();
        assert_eq!(saved(&path).transcript_size, Some(20));
        cx.update(|_, cx| {
            assert_eq!(cx.theme().transcript_text(), (px(20.), px(32.)));
            assert_eq!(TextSize::Base.px(), px(13.), "the interface keeps its size");
        });
    }

    #[gpui::test]
    fn settings_work_from_the_keyboard_alone(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        seeded(&path);
        let (view, cx) = open_window(&path, cx);

        keys(cx, "secondary-, down");
        assert!(shown(cx, "settings-ui-font"), "Down shows Fonts");
        keys(cx, "up");
        assert!(shown(cx, "settings-theme"), "Up shows Appearance");

        // Appearance's row holds focus; next come Fonts, Notifications, Sounds, Accounts,
        // Integrations, Prompts, Scheduled, Project, Theme, then the first action color.
        keys(cx, "tab tab tab tab tab tab tab tab tab tab");
        keys(cx, "right right");
        assert_eq!(saved(&path).action_color, keystone_core::ActionColor::Green);
        keys(cx, "left left");
        assert_eq!(saved(&path).action_color, keystone_core::ActionColor::Ink);

        // Past the six action colors to Compact.
        keys(cx, "tab tab tab tab tab tab");
        press(cx, "space");
        assert_eq!(saved(&path).density, Density::Compact);
        keys(cx, "right");
        assert_eq!(saved(&path).density, Density::Default);

        keys(cx, "secondary-k");
        assert!(
            shown(cx, "settings-page"),
            "the palette opens over the page"
        );
        keys(cx, "escape");
        assert!(shown(cx, "settings-page"), "Escape closes only the palette");

        // From Default back past Compact, the six action colors, Theme, Project, Scheduled,
        // Prompts, Integrations, Accounts, Sounds, Notifications, Fonts, and Appearance to Back.
        keys(cx, &["shift-tab"; 18].join(" "));
        press(cx, "space");
        assert!(!shown(cx, "settings-page"), "Back closes");
        assert!(composer_focused(&view, cx));
    }

    #[gpui::test]
    fn settings_tab_from_an_open_menu_closes_it_and_moves_on(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        seeded(&path);
        let (view, cx) = open_window(&path, cx);
        // Fonts' row holds focus; Notifications', Sounds', Accounts', Integrations', Prompts',
        // Scheduled's and Project's rows come before the first font menu.
        keys(cx, "secondary-, down tab tab tab tab tab tab tab tab down");
        assert!(shown(cx, "settings-ui-font-option-0"));

        keys(cx, "tab");
        assert!(
            !shown(cx, "settings-ui-font-option-0"),
            "Tab closes the menu"
        );
        keys(cx, "down");
        assert!(
            shown(cx, "settings-mono-font-option-0"),
            "Tab moved on to the next stop"
        );

        keys(cx, "shift-tab");
        assert!(
            !shown(cx, "settings-mono-font-option-0"),
            "Shift+Tab closes the menu"
        );
        cx.update(|window, cx| {
            let page = view.read(cx).settings.as_ref().unwrap();
            assert!(page.ui_font.is_focused(window), "Shift+Tab moved back");
        });

        keys(cx, "escape");
        assert!(!shown(cx, "settings-page"), "one Escape closes the page");
    }

    #[gpui::test]
    fn settings_shortcuts_still_work_after_closing_on_the_welcome_screen(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let (_view, cx) = open_window(&path, cx);
        for close in ["escape", "secondary-,", "secondary-l"] {
            keys(cx, "secondary-,");
            assert!(shown(cx, "settings-page"), "Ctrl+Comma opens");
            keys(cx, close);
            assert!(!shown(cx, "settings-page"), "{close} closes");
        }
        keys(cx, "secondary-,");
        assert!(shown(cx, "settings-page"), "Ctrl+Comma opens after Ctrl+L");
    }

    #[gpui::test]
    fn settings_cog_and_back_open_and_close_the_page(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        seeded(&path);
        let (view, cx) = open_window(&path, cx);
        let cog = cx.debug_bounds("settings").unwrap();
        click(cx, "customize-toggle");
        assert!(shown(cx, "customize-panel"));

        click(cx, "settings");
        assert!(shown(cx, "settings-page"));
        assert!(
            !shown(cx, "customize-panel"),
            "opening settings closes Customize"
        );
        assert!(!shown(cx, "customize-toggle"));
        assert_eq!(cx.debug_bounds("settings"), Some(cog), "the cog stays put");
        view.read_with(cx, |view, _cx| assert!(view.settings.is_some()));

        click(cx, "settings");
        assert!(!shown(cx, "settings-page"), "the page's cog closes it");
        assert!(composer_focused(&view, cx));

        click(cx, "settings");
        click(cx, "settings-back");
        assert!(!shown(cx, "settings-page"));
        assert!(composer_focused(&view, cx));
    }

    fn session_count(path: &Path) -> usize {
        Store::open(path).unwrap().workspace().sessions().len()
    }

    /// Escape and Ctrl+Comma still reach the window once the page has gone.
    fn settings_shortcuts_work(cx: &mut VisualTestContext) {
        keys(cx, "secondary-,");
        assert!(shown(cx, "settings-page"), "Ctrl+Comma opens again");
        keys(cx, "escape");
        assert!(!shown(cx, "settings-page"), "Escape closes again");
    }

    #[gpui::test]
    fn settings_files_commands_leave_the_page_and_keep_shortcuts(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        seeded(&path);
        let (_view, cx) = open_window(&path, cx);
        for palette in [false, true] {
            keys(cx, "secondary-,");
            if palette {
                keys(cx, "secondary-k");
                cx.simulate_input("Toggle files");
                keys(cx, "enter");
            } else {
                keys(cx, "secondary-shift-e");
            }
            assert!(!shown(cx, "settings-page"), "Files leaves Settings");
            assert!(shown(cx, "files-tab-0"), "the Files surface is visible");
            keys(cx, "secondary-shift-e");
            assert!(!shown(cx, "files-tab-0"), "focused Files closes");
            settings_shortcuts_work(cx);
        }
    }

    #[gpui::test]
    fn settings_notes_commands_leave_the_page(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        seeded(&path);
        let (_view, cx) = open_window(&path, cx);
        for palette in [false, true] {
            keys(cx, "secondary-,");
            if palette {
                keys(cx, "secondary-k");
                cx.simulate_input("Toggle notes");
                keys(cx, "enter");
            } else {
                keys(cx, "secondary-shift-j");
            }
            assert!(!shown(cx, "settings-page"), "Notes leaves Settings");
            assert!(shown(cx, "notes-editor"), "the Notes surface is visible");
            keys(cx, "secondary-shift-j");
            assert!(!shown(cx, "notes-editor"), "focused Notes closes");
            settings_shortcuts_work(cx);
        }
    }

    #[gpui::test]
    fn settings_files_reveal_uses_the_current_window_size(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        seeded(&path);
        let (view, cx) = open_window(&path, cx);
        cx.simulate_resize(gpui::size(px(1200.), px(900.)));
        keys(cx, "secondary-shift-e");
        for width in [400., 1200.] {
            keys(cx, "secondary-,");
            cx.simulate_resize(gpui::size(px(width), px(900.)));
            cx.run_until_parked();
            keys(cx, "secondary-shift-e");
            assert!(!shown(cx, "settings-page"));
            assert!(
                shown(cx, "files-tree-row-0"),
                "Files is revealed at {width}px"
            );
            view.update_in(cx, |view, window, cx| {
                let files = view
                    .shown_composition(cx)
                    .views()
                    .find(|(_, component)| *component == keystone_core::Component::Files)
                    .unwrap()
                    .0;
                assert!(view.view_contains_focus(files, window, cx));
            });
        }
    }

    #[gpui::test]
    fn settings_files_open_after_discarding_preview_only_conversations(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        seeded(&path);
        let (view, cx) = open_window(&path, cx);
        cx.simulate_resize(gpui::size(px(1400.), px(900.)));
        click(cx, "customize-toggle");
        click(cx, "component-duplicate");
        click(cx, "select-component-0");
        click(cx, "component-remove");
        assert!(!shown(cx, "conversation"));
        keys(cx, "secondary-, secondary-shift-e");
        assert!(!shown(cx, "settings-page"));
        assert!(
            shown(cx, "files-tree-row-0"),
            "Files opens on the first command"
        );
        view.read_with(cx, |view, cx| {
            assert!(view.error.is_none(), "no undeclared preview target is used");
            assert_eq!(view.composer.read(cx).value(), DRAFT);
        });
        settings_shortcuts_work(cx);
    }

    #[gpui::test]
    fn settings_closes_the_workspace_surface_menu(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        seeded(&path);
        let (view, cx) = open_window(&path, cx);
        keys(cx, "secondary-shift-e");
        click(cx, "surface-add");
        assert!(shown(cx, "surface-add-conversation"));
        keys(cx, "secondary-,");
        assert!(shown(cx, "settings-page"));
        view.read_with(cx, |view, _cx| {
            assert!(view.presentation.current.arrangement.surface_menu.is_none());
        });
        keys(cx, "escape");
        assert!(!shown(cx, "surface-add-conversation"));
    }

    #[gpui::test]
    fn settings_new_session_shortcut_leaves_the_page_for_the_new_session(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        seeded(&path);
        let (view, cx) = open_window(&path, cx);
        keys(cx, "secondary-, secondary-n");
        assert!(!shown(cx, "settings-page"), "Ctrl+N leaves settings");
        assert_eq!(session_count(&path), 2);
        assert!(shown(cx, "session-2"), "the new session is on screen");
        assert!(composer_focused(&view, cx));
        keys(cx, "h i");
        view.read_with(cx, |view, cx| {
            assert_eq!(view.composer.read(cx).value(), "hi")
        });
        settings_shortcuts_work(cx);
    }

    #[gpui::test]
    fn settings_palette_new_session_leaves_the_page(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        seeded(&path);
        let (view, cx) = open_window(&path, cx);
        keys(cx, "secondary-, secondary-k");
        cx.simulate_input("New Claude session");
        keys(cx, "enter");
        assert!(!shown(cx, "settings-page"));
        assert_eq!(session_count(&path), 2);
        assert!(composer_focused(&view, cx));
        settings_shortcuts_work(cx);
    }

    #[gpui::test]
    fn settings_focus_search_leaves_the_page_for_search(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        seeded(&path);
        let (view, cx) = open_window(&path, cx);
        keys(cx, "secondary-, secondary-f");
        assert!(!shown(cx, "settings-page"));
        cx.update(|window, cx| assert!(view.read(cx).search.focus_handle(cx).is_focused(window)));
        settings_shortcuts_work(cx);
    }

    #[gpui::test]
    fn settings_rename_leaves_the_page_and_shortcuts_survive_it(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        seeded(&path);
        let (view, cx) = open_window(&path, cx);
        keys(cx, "secondary-, f2");
        assert!(!shown(cx, "settings-page"));
        view.read_with(cx, |view, _cx| assert!(view.renaming.is_some()));
        keys(cx, "escape");
        view.read_with(cx, |view, _cx| assert!(view.renaming.is_none()));
        settings_shortcuts_work(cx);
    }

    #[gpui::test]
    fn settings_toggle_sidebar_leaves_the_page_and_toggles(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        seeded(&path);
        let (_view, cx) = open_window(&path, cx);
        assert!(shown(cx, "session-1"));
        keys(cx, "secondary-, secondary-b");
        assert!(!shown(cx, "settings-page"));
        assert!(!shown(cx, "session-1"), "the sidebar is hidden");
        settings_shortcuts_work(cx);
    }

    #[gpui::test]
    fn settings_sidebar_button_leaves_the_page_and_toggles(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        seeded(&path);
        let (_view, cx) = open_window(&path, cx);
        keys(cx, "secondary-,");
        click(cx, "sidebar-toggle");
        assert!(!shown(cx, "settings-page"));
        assert!(!shown(cx, "session-1"), "the sidebar is hidden");
        click(cx, "sidebar-toggle");
        assert!(shown(cx, "session-1"), "the sidebar is back");
        settings_shortcuts_work(cx);
    }

    #[gpui::test]
    fn settings_customize_shortcut_leaves_the_page_for_customize(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        seeded(&path);
        let (_view, cx) = open_window(&path, cx);
        keys(cx, "secondary-, secondary-shift-l");
        assert!(!shown(cx, "settings-page"));
        assert!(shown(cx, "customize-panel"));
        keys(cx, "escape");
        assert!(!shown(cx, "customize-panel"));
        settings_shortcuts_work(cx);
    }

    #[gpui::test]
    fn settings_send_shortcut_sends_nothing(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        seeded(&path);
        let (view, cx) = open_window(&path, cx);
        keys(cx, "secondary-, secondary-enter");
        assert!(shown(cx, "settings-page"), "the page stays");
        view.read_with(cx, |view, cx| {
            assert_eq!(view.composer.read(cx).value(), DRAFT);
            let conversation = &view.state.read(cx).conversations()[&1];
            assert!(conversation.rows.is_empty(), "no turn started");
            assert_eq!(conversation.error, None, "no send was attempted");
        });
        keys(cx, "escape");
        assert!(!shown(cx, "settings-page"));
    }

    #[gpui::test]
    #[ignore = "benchmark"]
    fn perf_preferences_apply(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        seeded(&path);
        let (_view, cx) = open_window(&path, cx);
        keys(cx, "secondary-,");
        let mut samples = Vec::new();
        for iteration in 0..50 {
            let selector = if iteration % 2 == 0 {
                "density-compact"
            } else {
                "density-default"
            };
            let center = cx.debug_bounds(selector).unwrap().center();
            let start = std::time::Instant::now();
            cx.simulate_click(center, Modifiers::none());
            cx.update(|window, cx| window.draw(cx).clear(cx));
            samples.push(start.elapsed());
            cx.run_until_parked();
        }
        samples.sort();
        let micros = |at: usize| samples[at].as_micros();
        println!(
            "PERF preferences_apply_us median={} p95={} n={}",
            micros(samples.len() / 2),
            micros(samples.len() * 95 / 100),
            samples.len()
        );
    }
}
