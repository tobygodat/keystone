//! Settings → Prompts, the prompt library: every saved prompt in the user's order,
//! with a form to add or edit one. A prompt is offered in every project or only in
//! windows with one project open; `/shortcut` finds it in the composer's menu, where a provider
//! command of the same name wins.

use crate::Keystone;
use crate::prompts::prompt_detail;
use crate::settings_page::SettingsPage;
use gpui::{
    AnyElement, App, AppContext, Context, Entity, FocusHandle, Focusable, FontWeight, KeyDownEvent,
    Window, div, prelude::*, px,
};
use keystone_agent::{Commands, terminal_only};
use keystone_core::{ProjectId, Prompt, PromptFields, Provider};
use keystone_ui::prelude::*;
use keystone_ui::text_input::{InputMode, TextInput};
use std::cell::RefCell;
use std::collections::HashMap;

/// The section's own state, kept while the settings page is open.
pub(crate) struct PromptsPage {
    title: Entity<TextInput>,
    shortcut: Entity<TextInput>,
    body: Entity<TextInput>,
    /// The project the form saves to, or `None` for every project.
    project: Option<ProjectId>,
    scope_focus: FocusHandle,
    save: FocusHandle,
    cancel: FocusHandle,
    /// The prompt the form edits, as it was loaded, or `None` while it adds one.
    editing: Option<Prompt>,
    /// Focus for each row's buttons, made as rows first render.
    stops: RefCell<HashMap<String, FocusHandle>>,
}

fn field(placeholder: &str, mode: InputMode, cx: &mut App) -> Entity<TextInput> {
    let input = cx.new(|cx| TextInput::new(placeholder, mode, cx));
    let _ = input.read(cx).focus_handle(cx).tab_stop(true);
    input
}

impl PromptsPage {
    pub(crate) fn new(cx: &mut App) -> Self {
        Self {
            title: field("Title", InputMode::SingleLine, cx),
            shortcut: field("/shortcut", InputMode::SingleLine, cx),
            body: field("Text", InputMode::Multiline, cx),
            project: None,
            scope_focus: cx.focus_handle().tab_stop(true),
            save: cx.focus_handle().tab_stop(true),
            cancel: cx.focus_handle().tab_stop(true),
            editing: None,
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
    fn prompts_page(&mut self) -> Option<&mut PromptsPage> {
        self.settings.as_mut().map(|page| &mut page.prompts)
    }

    /// The scopes the form offers: every project, this window's project, and the
    /// edited prompt's own project when that is another.
    fn prompt_scopes(&self, page: &PromptsPage) -> Vec<Option<ProjectId>> {
        let mut scopes = vec![None];
        for project in [
            self.presentation.project.as_ref().ok().cloned(),
            page.project.clone(),
        ]
        .into_iter()
        .flatten()
        {
            if !scopes.contains(&Some(project.clone())) {
                scopes.push(Some(project));
            }
        }
        scopes
    }

    /// The providers with a command that shares a prompt's shortcut, in any case: one
    /// the selected session's account listed, else a Terminal only built-in.
    fn shortcut_conflicts(&self, shortcut: &str, cx: &App) -> Vec<Provider> {
        let invocation = format!("/{shortcut}");
        let state = self.state.read(cx);
        let session = self.selected(cx);
        [Provider::Claude, Provider::Codex]
            .into_iter()
            .filter(|&provider| {
                let listed = session
                    .and_then(|id| state.commands(id, provider))
                    .is_some_and(|listed| {
                        matches!(listed, Commands::Ready(commands)
                            if commands.iter().any(|c| c.invocation.eq_ignore_ascii_case(&invocation)))
                    });
                listed
                    || terminal_only(provider)
                        .iter()
                        .any(|c| c.invocation.eq_ignore_ascii_case(&invocation))
            })
            .collect()
    }

    /// The Prompts section's content, below its title.
    pub(crate) fn prompts_section(
        &self,
        page: &SettingsPage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let prompts = self.state.read(cx).store().workspace().prompts().to_vec();
        if let Some(session) = self.selected_session(cx) {
            // Shortcut conflicts need the session's command list; asked once.
            let (id, provider) = (session.id(), session.provider());
            let state = self.state.read(cx);
            if prompts.iter().any(|p| p.shortcut().is_some())
                && state.catalog_key(id, provider).is_some()
                && state.commands(id, provider).is_none()
            {
                let view = cx.weak_entity();
                cx.defer(move |cx| {
                    let _ = view.update(cx, |this, cx| this.request_catalog(id, provider, cx));
                });
            }
        }
        let count = prompts.len();
        let rows: Vec<_> = prompts
            .iter()
            .enumerate()
            .map(|(at, prompt)| self.prompt_row(&page.prompts, prompt, at, count, cx))
            .collect();
        v_flex()
            .automation_id("settings-prompts")
            .child(v_flex().pt(spacing::MD).children(rows))
            .child(self.prompt_form(&page.prompts, cx))
            .into_any_element()
    }

    fn prompt_row(
        &self,
        page: &PromptsPage,
        prompt: &Prompt,
        at: usize,
        count: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let id = prompt.id();
        let button = |name: &str, icon: IconName, label: &'static str, disabled: bool| {
            let selector = format!("prompt-{name}-{id}");
            IconButton::new(selector.clone(), icon)
                .style(ButtonStyle::Subtle)
                .size(ButtonSize::Compact)
                .disabled(disabled)
                .track_focus(&page.stop(&selector, cx))
                .debug_selector(move || selector.clone())
                .aria_label(label)
                .tooltip(Tooltip::text(label))
        };
        let actions = h_flex()
            .flex_none()
            .gap_1()
            .child(button("up", IconName::Up, "Move up", at == 0).on_click(
                self.scoped_listener(cx, move |this, _, _, cx| {
                    this.move_prompt(id, at.saturating_sub(1), cx)
                }),
            ))
            .child(
                button("down", IconName::Down, "Move down", at + 1 == count).on_click(
                    self.scoped_listener(cx, move |this, _, _, cx| {
                        this.move_prompt(id, at + 1, cx)
                    }),
                ),
            )
            .child(button("edit", IconName::Pencil, "Edit", false).on_click(
                self.scoped_listener(cx, move |this, _, window, cx| {
                    this.edit_prompt(id, window, cx)
                }),
            ))
            .child(button("delete", IconName::Close, "Delete", false).on_click(
                self.scoped_listener(cx, move |this, _, window, cx| {
                    this.delete_prompt(id, window, cx)
                }),
            ));
        let conflicts = prompt
            .shortcut()
            .map(|shortcut| (shortcut, self.shortcut_conflicts(shortcut, cx)))
            .filter(|(_, providers)| !providers.is_empty())
            .map(|(shortcut, providers)| {
                providers
                    .iter()
                    .map(|&provider| {
                        let name = crate::model_selector::provider_name(provider);
                        format!("{name} also has /{shortcut}.")
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            });
        let detail = prompt_detail(prompt);
        let editing = page.editing.as_ref().is_some_and(|p| p.id() == id);
        h_flex()
            .automation_id(format!("prompt-row-{id}"))
            .gap(spacing::MD)
            .py(spacing::SM)
            .hairline_t(cx)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        Label::new(prompt.title().to_owned())
                            .weight(if editing {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::NORMAL
                            })
                            .truncate(),
                    )
                    .when(!detail.is_empty(), |d| {
                        d.child(
                            Label::new(detail)
                                .size(TextSize::Sm)
                                .color(Color::Faint)
                                .truncate(),
                        )
                    })
                    .children(conflicts.map(|text| {
                        div()
                            .automation_id(format!("prompt-conflict-{id}"))
                            .child(Label::new(text).size(TextSize::Sm).color(Color::Warning))
                    })),
            )
            .child(actions)
    }

    fn prompt_form(&self, page: &PromptsPage, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let colors = cx.theme().colors;
        let boxed = |entity: &Entity<TextInput>, id: &'static str| {
            div()
                .flex_1()
                .min_w(px(120.))
                .px(px(8.))
                .rounded(px(6.))
                .border_1()
                .border_color(colors.border)
                .bg(colors.background)
                .automation_id(id)
                .child(entity.clone())
        };
        let submit = self.scoped_listener(cx, |this, event: &KeyDownEvent, window, cx| {
            if event.keystroke.key == "enter" && !event.keystroke.modifiers.shift {
                this.save_prompt(window, cx);
                cx.stop_propagation();
            }
        });
        let scopes = self.prompt_scopes(page);
        let labels: Vec<String> = scopes
            .iter()
            .map(|scope| {
                scope
                    .as_ref()
                    .map_or_else(|| "All projects".to_owned(), |p| p.name().to_owned())
            })
            .collect();
        let selected = scopes.iter().position(|scope| *scope == page.project);
        let editing = page.editing.is_some();
        v_flex()
            .pt(spacing::LG)
            .gap(spacing::SM)
            .automation_id("prompt-form")
            .child(
                h_flex()
                    .flex_wrap()
                    .gap(spacing::SM)
                    .on_key_down(submit)
                    .child(boxed(&page.title, "prompt-title"))
                    .child(
                        div()
                            .w(px(160.))
                            .flex_none()
                            .child(boxed(&page.shortcut, "prompt-shortcut")),
                    ),
            )
            .child({
                let body = page.body.clone();
                boxed(&page.body, "prompt-body")
                    .min_h(px(88.))
                    .py(px(6.))
                    .cursor_text()
                    .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
                        body.focus_handle(cx).focus(window, cx);
                        cx.stop_propagation();
                    })
            })
            .child(
                h_flex()
                    .flex_wrap()
                    .gap(spacing::SM)
                    .child(
                        div().flex_none().child(
                            Select::new("prompt-scope", labels, selected)
                                .width(px(220.))
                                .track_focus(&page.scope_focus)
                                .on_change(self.scoped_listener(
                                    cx,
                                    move |this, &at: &usize, _, cx| {
                                        let scope = scopes.get(at).cloned().flatten();
                                        if let Some(page) = this.prompts_page() {
                                            page.project = scope;
                                        }
                                        cx.notify();
                                    },
                                )),
                        ),
                    )
                    .child(div().flex_1())
                    .when(editing, |d| {
                        d.child(
                            Button::new("prompt-cancel", "Cancel")
                                .style(ButtonStyle::Subtle)
                                .track_focus(&page.cancel)
                                .debug_selector(|| "prompt-cancel".into())
                                .on_click(self.scoped_listener(cx, |this, _, window, cx| {
                                    this.reset_prompt_form(window, cx)
                                })),
                        )
                    })
                    .child(
                        Button::new("prompt-save", if editing { "Save" } else { "Add" })
                            .style(ButtonStyle::Filled)
                            .track_focus(&page.save)
                            .debug_selector(|| "prompt-save".into())
                            .on_click(self.scoped_listener(cx, |this, _, window, cx| {
                                this.save_prompt(window, cx)
                            })),
                    ),
            )
    }

    fn save_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(page) = &self.settings else { return };
        let page = &page.prompts;
        let fields = PromptFields {
            title: page.title.read(cx).value().to_owned(),
            body: page.body.read(cx).value().to_owned(),
            shortcut: page.shortcut.read(cx).value().to_owned(),
            project: page.project.clone(),
        };
        let editing = page.editing.as_ref().map(Prompt::id);
        if let Some(loaded) = &page.editing {
            match self.state.read(cx).store().workspace().prompt(loaded.id()) {
                None => {
                    self.reset_prompt_form(window, cx);
                    self.error = Some("Prompt deleted in another window.".into());
                    return self.refresh(cx);
                }
                Some(stored) if !stored.same_content(loaded) => {
                    self.error = Some("Prompt changed in another window. Edit it again.".into());
                    return self.refresh(cx);
                }
                Some(_) => {}
            }
        }
        let result = self.state.update(cx, |state, cx| {
            state.change_prompts(
                |store| match editing {
                    Some(id) => store.edit_prompt(id, fields),
                    None => store
                        .add_prompt(fields)
                        .map(|applied| keystone_core::Applied {
                            value: (),
                            durability: applied.durability,
                            revision: applied.revision,
                        }),
                },
                cx,
            )
        });
        match result {
            Ok(applied) => {
                self.error = crate::durability_warning(applied.durability);
                self.reset_prompt_form(window, cx);
            }
            Err(error) => self.error = Some(format!("Prompt not saved. {error}")),
        }
        self.refresh(cx);
    }

    /// Empties the form for a new prompt and focuses its title.
    fn reset_prompt_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(page) = self.prompts_page() else {
            return;
        };
        page.editing = None;
        page.project = None;
        let inputs = [page.title.clone(), page.shortcut.clone(), page.body.clone()];
        for input in &inputs {
            input.update(cx, |input, cx| input.set_value(String::new(), cx));
        }
        inputs[0].focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn edit_prompt(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prompt) = self.state.read(cx).store().workspace().prompt(id).cloned() else {
            return;
        };
        let Some(page) = self.prompts_page() else {
            return;
        };
        page.editing = Some(prompt.clone());
        page.project = prompt.project();
        let (title, shortcut, body) =
            (page.title.clone(), page.shortcut.clone(), page.body.clone());
        title.update(cx, |input, cx| {
            input.set_value(prompt.title().to_owned(), cx)
        });
        shortcut.update(cx, |input, cx| {
            input.set_value(
                prompt
                    .shortcut()
                    .map(|s| format!("/{s}"))
                    .unwrap_or_default(),
                cx,
            )
        });
        body.update(cx, |input, cx| {
            input.set_value(prompt.body().to_owned(), cx)
        });
        title.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn move_prompt(&mut self, id: u64, to: usize, cx: &mut Context<Self>) {
        self.error = match self.state.update(cx, |state, cx| {
            state.change_prompts(|store| store.move_prompt(id, to), cx)
        }) {
            Ok(applied) => crate::durability_warning(applied.durability),
            Err(error) => Some(format!("Prompt not moved. {error}")),
        };
        self.refresh(cx);
    }

    fn delete_prompt(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let result = self.state.update(cx, |state, cx| {
            state.change_prompts(|store| store.remove_prompt(id), cx)
        });
        match result {
            Ok(applied) => {
                self.error = crate::durability_warning(applied.durability);
                if self
                    .settings
                    .as_ref()
                    .is_some_and(|page| page.prompts.editing.as_ref().is_some_and(|p| p.id() == id))
                {
                    self.reset_prompt_form(window, cx);
                }
            }
            Err(error) => self.error = Some(format!("Prompt not deleted. {error}")),
        }
        self.refresh(cx);
    }
}

#[cfg(all(test, feature = "ui-tests"))]
#[path = "settings_prompts_tests.rs"]
mod ui_tests;
