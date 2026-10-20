//! The settings page: it takes the place of the workspace body and edits the saved
//! appearance, one section at a time.

use crate::customization::OpenCustomize;
use crate::settings::{self, DEFAULT_TEXT_SIZE, TEXT_SIZES};
use crate::{
    FocusComposer, FocusSearch, Keystone, NewClaude, NewCodex, RenameSession, SendMessage,
    ToggleSettle, ToggleSidebar,
};
use gpui::{
    AnyElement, App, Context, FocusHandle, FontWeight, KeyDownEvent, Window, div, prelude::*, px,
};
use keystone_core::{Appearance, Density, SidebarWidth, ThemeMode};
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
    Density,
    TextSize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Appearance,
    Fonts,
}

/// A section's rows, each a label and its control.
type Rows = &'static [(&'static str, Field)];

/// Each section with its label, automation name, and rows, in sidebar order.
const SECTIONS: [(Section, &str, &str, Rows); 2] = [
    (
        Section::Appearance,
        "Appearance",
        "settings-section-appearance",
        &[
            ("Theme", Field::Theme),
            ("Density", Field::Density),
            ("Text size", Field::TextSize),
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
    density: [FocusHandle; DENSITIES.len()],
    text_size: FocusHandle,
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
            density: std::array::from_fn(|_| stop()),
            text_size: stop(),
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
        self.model_menu = None;
        self.presentation.current.arrangement.surface_menu = None;
        let page = SettingsPage::new(cx);
        window.focus(&page.sections[page.section_index()], cx);
        self.settings = Some(page);
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
            .capture_action(self.leave_settings::<OpenCustomize>(cx))
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

    fn show_section(&mut self, at: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(page) = &mut self.settings else {
            return;
        };
        page.section = SECTIONS[at].0;
        window.focus(&page.sections[at], cx);
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
        let rows = rows.iter().enumerate().map(|(row, &(label, field))| {
            h_flex()
                .gap(spacing::MD)
                .py(spacing::MD)
                .when(row > 0, |d| d.hairline_t(cx))
                .child(div().flex_1().min_w_0().child(Label::new(label).truncate()))
                .child(self.control(page, field, appearance, cx))
        });
        let content = div()
            .id("settings-content")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(560.))
                    .p(spacing::XL)
                    .child(
                        div().pb(spacing::SM).child(
                            Label::new(title)
                                .size(TextSize::Body)
                                .weight(FontWeight::SEMIBOLD)
                                .color(Color::Strong),
                        ),
                    )
                    .children(rows)
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
                let Some(at) = focused(&page.density) else {
                    return false;
                };
                let next = if keystroke.key == "right" {
                    (at + 1).min(DENSITIES.len() - 1)
                } else {
                    at.saturating_sub(1)
                };
                window.focus(&page.density[next], cx);
                let density = DENSITIES[next].0;
                if self
                    .state
                    .read(cx)
                    .store()
                    .workspace()
                    .preferences()
                    .appearance()
                    .density
                    != density
                {
                    self.update_appearance(|appearance| appearance.density = density, window, cx);
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
    use keystone_core::{Appearance, Density, Store, ThemeMode};
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
        keys(cx, "ctrl-,");
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
        keys(cx, "ctrl-,");
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        click(cx, "settings-theme");
        click(cx, "settings-theme-option-1");
        view.read_with(cx, |view, cx| {
            assert!(
                view.error
                    .as_deref()
                    .unwrap()
                    .starts_with("Settings not saved.")
            );
            assert_eq!(
                view.state
                    .read(cx)
                    .store()
                    .workspace()
                    .preferences()
                    .appearance()
                    .theme,
                ThemeMode::System
            );
        });
        assert!(shown(cx, "settings-error"), "the page shows the save error");
        std::fs::remove_dir(&path).unwrap();
        click(cx, "settings-theme");
        click(cx, "settings-theme-option-1");
        assert_eq!(saved(&path).theme, ThemeMode::Light);
        assert!(
            !shown(cx, "settings-error"),
            "a successful save clears the error"
        );
    }

    #[gpui::test]
    fn settings_changes_persist_preferences(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        seeded(&path);
        let (view, cx) = open_window(&path, cx);
        assert_eq!(height(cx, "settings"), px(32.));
        let row = height(cx, "session-1");
        assert!(!shown(cx, "settings-page"));

        keys(cx, "ctrl-,");
        assert!(shown(cx, "settings-page"), "Ctrl+Comma opens");
        assert!(!shown(cx, "session-1"), "the page replaces the workspace");

        click(cx, "settings-theme");
        click(cx, "settings-theme-option-1");
        assert_eq!(saved(&path).theme, ThemeMode::Light);
        cx.update(|_, cx| assert_eq!(cx.theme().appearance, keystone_ui::Appearance::Light));

        click(cx, "density-compact");
        assert_eq!(saved(&path).density, Density::Compact);
        cx.update(|_, cx| assert_eq!(cx.theme().density, keystone_ui::Density::Compact));
        assert_eq!(height(cx, "settings"), px(28.));

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

        keys(cx, "ctrl-,");
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
    fn settings_work_from_the_keyboard_alone(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        seeded(&path);
        let (view, cx) = open_window(&path, cx);

        keys(cx, "ctrl-, down");
        assert!(shown(cx, "settings-ui-font"), "Down shows Fonts");
        keys(cx, "up");
        assert!(shown(cx, "settings-theme"), "Up shows Appearance");

        // Appearance's row holds focus; next come Fonts, Theme, then Compact.
        keys(cx, "tab tab tab");
        press(cx, "space");
        assert_eq!(saved(&path).density, Density::Compact);
        keys(cx, "right");
        assert_eq!(saved(&path).density, Density::Default);

        keys(cx, "ctrl-k");
        assert!(
            shown(cx, "settings-page"),
            "the palette opens over the page"
        );
        keys(cx, "escape");
        assert!(shown(cx, "settings-page"), "Escape closes only the palette");

        // From Default back past Compact, Theme, Fonts, and Appearance to Back.
        keys(cx, "shift-tab shift-tab shift-tab shift-tab shift-tab");
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
        keys(cx, "ctrl-, down tab down");
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
        for close in ["escape", "ctrl-,", "ctrl-l"] {
            keys(cx, "ctrl-,");
            assert!(shown(cx, "settings-page"), "Ctrl+Comma opens");
            keys(cx, close);
            assert!(!shown(cx, "settings-page"), "{close} closes");
        }
        keys(cx, "ctrl-,");
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
        keys(cx, "ctrl-,");
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
            keys(cx, "ctrl-,");
            if palette {
                keys(cx, "ctrl-k");
                cx.simulate_input("Toggle files");
                keys(cx, "enter");
            } else {
                keys(cx, "ctrl-shift-e");
            }
            assert!(!shown(cx, "settings-page"), "Files leaves Settings");
            assert!(shown(cx, "files-tab-0"), "the Files surface is visible");
            keys(cx, "ctrl-shift-e");
            assert!(!shown(cx, "files-tab-0"), "focused Files closes");
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
        keys(cx, "ctrl-shift-e");
        for width in [400., 1200.] {
            keys(cx, "ctrl-,");
            cx.simulate_resize(gpui::size(px(width), px(900.)));
            cx.run_until_parked();
            keys(cx, "ctrl-shift-e");
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
        keys(cx, "ctrl-, ctrl-shift-e");
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
        keys(cx, "ctrl-shift-e");
        click(cx, "surface-add");
        assert!(shown(cx, "surface-add-conversation"));
        keys(cx, "ctrl-,");
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
        keys(cx, "ctrl-, ctrl-n");
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
        keys(cx, "ctrl-, ctrl-k");
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
        keys(cx, "ctrl-, ctrl-f");
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
        keys(cx, "ctrl-, f2");
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
        keys(cx, "ctrl-, ctrl-b");
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
        keys(cx, "ctrl-,");
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
        keys(cx, "ctrl-, ctrl-shift-l");
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
        keys(cx, "ctrl-, ctrl-enter");
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
        keys(cx, "ctrl-,");
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
