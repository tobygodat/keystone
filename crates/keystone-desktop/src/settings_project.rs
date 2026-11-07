//! Settings → Project: what the window's project does for itself. Today that is its
//! worktree setup script, which runs in each new worktree before its first message.

use crate::Keystone;
use crate::settings_page::SettingsPage;
use gpui::{
    AnyElement, App, AppContext, Context, Entity, FocusHandle, Focusable, FontWeight, Window, div,
    prelude::*, px,
};
use keystone_core::ProjectId;
use keystone_ui::prelude::*;
use keystone_ui::text_input::{InputMode, TextInput};

/// The section's own state, kept while the settings page is open.
pub(crate) struct ProjectPage {
    script: Entity<TextInput>,
    /// The project whose script the field was filled from, and which Save writes to.
    loaded_for: Option<ProjectId>,
    save: FocusHandle,
    error: Option<String>,
}

impl ProjectPage {
    pub(crate) fn new(cx: &mut App) -> Self {
        let script = cx.new(|cx| TextInput::new("npm install", InputMode::Multiline, cx));
        let _ = script.read(cx).focus_handle(cx).tab_stop(true);
        Self {
            script,
            loaded_for: None,
            save: cx.focus_handle().tab_stop(true),
            error: None,
        }
    }
}

impl Keystone {
    fn saved_setup_script(&self, cx: &App) -> Option<String> {
        let project = self.presentation.project.as_ref().ok()?;
        let state = self.state.read(cx);
        Some(
            state
                .store()
                .workspace()
                .setup_script(project)
                .unwrap_or_default()
                .to_owned(),
        )
    }

    /// Fills the field with the window's project's saved script.
    pub(crate) fn load_project_settings(&mut self, cx: &mut Context<Self>) {
        let saved = self.saved_setup_script(cx).unwrap_or_default();
        let project = self.presentation.project.as_ref().ok().cloned();
        let Some(page) = &mut self.settings else {
            return;
        };
        page.project.loaded_for = project;
        page.project.error = None;
        page.project
            .script
            .update(cx, |input, cx| input.set_value(saved, cx));
    }

    /// Saves the field to the project it was filled from. If the window's project has
    /// changed since, the field is filled from the new one instead.
    fn save_setup_script(&mut self, cx: &mut Context<Self>) {
        let Some(page) = &self.settings else {
            return;
        };
        if page.project.loaded_for.as_ref() != self.presentation.project.as_ref().ok() {
            self.load_project_settings(cx);
            return self.refresh(cx);
        }
        let Some(project) = page.project.loaded_for.clone() else {
            return;
        };
        let script = page.project.script.read(cx).value().to_owned();
        let result = self.state.update(cx, |state, cx| {
            state.set_project_setup_script(&project, script, cx)
        });
        // A failed disk write comes later and shows in the status line, like every save.
        let error = result
            .err()
            .map(|error| format!("Setup script not saved. {error}"));
        let saved = error.is_none();
        if let Some(page) = &mut self.settings {
            page.project.error = error;
        }
        if saved {
            self.load_project_settings(cx);
        }
        self.refresh(cx);
    }

    /// The Project section's content, below its title.
    pub(crate) fn project_section(
        &self,
        page: &SettingsPage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Ok(project) = &self.presentation.project else {
            return div()
                .automation_id("settings-project")
                .child(Label::new("Open a project to set its setup script.").color(Color::Muted))
                .into_any_element();
        };
        let page = &page.project;
        let colors = cx.theme().colors;
        let saved = self.saved_setup_script(cx).unwrap_or_default();
        let changed = page.script.read(cx).value().trim() != saved;
        let script = page.script.clone();
        v_flex()
            .automation_id("settings-project")
            .gap(spacing::SM)
            .child(
                v_flex()
                    .pb(spacing::SM)
                    .child(
                        Label::new(project.name().to_owned())
                            .weight(FontWeight::MEDIUM)
                            .color(Color::Strong),
                    )
                    .child(
                        Label::new(crate::projects::display_path(project.path()))
                            .size(TextSize::Sm)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
            .child(Label::new("Worktree setup script"))
            .child(
                div()
                    .automation_id("settings-setup-script")
                    .min_h(px(120.))
                    .px(px(8.))
                    .py(px(6.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.background)
                    .font_mono(cx)
                    .cursor_text()
                    .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
                        script.focus_handle(cx).focus(window, cx);
                        cx.stop_propagation();
                    })
                    .child(page.script.clone()),
            )
            .child(
                Label::new(
                    "Runs in each new worktree's folder before its first message. \
                     KEYSTONE_PROJECT_ROOT and KEYSTONE_WORKTREE_PATH are set.",
                )
                .size(TextSize::Sm)
                .color(Color::Muted),
            )
            .child(
                h_flex()
                    .gap(spacing::SM)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .children(page.error.clone().map(|error| {
                                div().automation_id("settings-setup-script-error").child(
                                    Label::new(error).size(TextSize::Sm).color(Color::Danger),
                                )
                            })),
                    )
                    .child(
                        Button::new("settings-setup-script-save", "Save")
                            .style(ButtonStyle::Filled)
                            .disabled(!changed)
                            .track_focus(&page.save)
                            .debug_selector(|| "settings-setup-script-save".into())
                            .on_click(
                                self.scoped_listener(cx, |this, _, _, cx| {
                                    this.save_setup_script(cx)
                                }),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// Opens the settings page on Project, its setup script field focused.
    pub(crate) fn open_project_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_project_section(window, cx);
        if self.presentation.project.is_ok()
            && let Some(page) = &self.settings
        {
            page.project.script.focus_handle(cx).focus(window, cx);
        }
    }
}
