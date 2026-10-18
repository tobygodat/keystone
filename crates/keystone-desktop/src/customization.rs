//! Customizing the sidebar: a preview the user edits directly or asks a provider to
//! propose, then applies, discards, or undoes. [`Customization`] holds every
//! transition; the view in this file renders it and runs its effects.

use crate::customization_agent::{Agent, Ask, Outcome, Programs, Proposal, REQUEST_TIMEOUT};
use crate::{Keystone, SendMessage, durability_warning, provider_name};
use gpui::{
    Context, Empty, Focusable, FontWeight, IntoElement, MouseButton, Window, div, prelude::*, px,
};
use keystone_core::{Provider, SavedLayout, SidebarLayout, SidebarSide, SidebarWidth};
use keystone_ui::prelude::*;
use std::path::PathBuf;

const WIDTH_STEP: f32 = 10.;
const PANEL_WIDTH: f32 = 340.;
const PANEL_TOP: f32 = 52.;
const PANEL_MARGIN: f32 = 16.;

gpui::actions!(
    customize,
    [SideLeft, SideRight, Narrower, Wider, ApplyLayout]
);

pub fn bind_keys(cx: &mut gpui::App) {
    cx.bind_keys([
        gpui::KeyBinding::new("alt-left", SideLeft, Some("Customize")),
        gpui::KeyBinding::new("alt-right", SideRight, Some("Customize")),
        gpui::KeyBinding::new("alt-[", Narrower, Some("Customize")),
        gpui::KeyBinding::new("alt-]", Wider, Some("Customize")),
        gpui::KeyBinding::new("ctrl-s", ApplyLayout, Some("Customize")),
    ]);
}

#[derive(Clone, Debug, PartialEq)]
pub enum Interpretation {
    Idle,
    Running { request: u64, revision: u64 },
    Failed(String),
    Unsupported(String),
}

#[derive(Clone, Debug, PartialEq)]
struct Editing {
    candidate: SidebarLayout,
    revision: u64,
    interpretation: Interpretation,
    note: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Open,
    SetSide(SidebarSide),
    Resize(SidebarWidth),
    Reset,
    Ask { provider: Provider, prompt: String },
    Retry,
    Cancel,
    Apply,
    Undo,
    Discard,
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Commit {
    Apply,
    Undo,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Ask(Ask),
    Cancel(u64),
    Save(SavedLayout, Commit),
}

#[derive(Debug, Default)]
pub struct Customization {
    editing: Option<Editing>,
    replaced_layout: Option<SavedLayout>,
    last_ask: Option<(Provider, String)>,
    next_request: u64,
}

impl Customization {
    pub fn is_open(&self) -> bool {
        self.editing.is_some()
    }

    /// The layout to draw: the preview while customizing, else the saved one.
    pub fn shown(&self, saved: &SavedLayout) -> SidebarLayout {
        self.editing
            .as_ref()
            .map_or_else(|| saved.shown(), |e| e.candidate)
    }

    pub fn interpretation(&self) -> Option<&Interpretation> {
        self.editing.as_ref().map(|e| &e.interpretation)
    }

    /// The provider the running request went to.
    pub fn asking(&self) -> Option<Provider> {
        match self.interpretation() {
            Some(Interpretation::Running { .. }) => self.last_ask.as_ref().map(|(p, _)| *p),
            _ => None,
        }
    }

    pub fn note(&self) -> Option<&str> {
        self.editing.as_ref().and_then(|e| e.note.as_deref())
    }

    pub fn can_retry(&self) -> bool {
        self.last_ask.is_some()
            && matches!(
                self.interpretation(),
                Some(Interpretation::Failed(_) | Interpretation::Unsupported(_))
            )
    }

    pub fn can_apply(&self, saved: &SavedLayout) -> bool {
        self.editing
            .as_ref()
            .is_some_and(|e| SavedLayout::from(e.candidate) != *saved)
    }

    pub fn can_discard(&self, saved: &SavedLayout) -> bool {
        self.editing
            .as_ref()
            .is_some_and(|e| e.candidate != saved.shown())
    }

    pub fn can_reset(&self, saved: &SavedLayout) -> bool {
        self.editing.as_ref().is_some_and(|e| {
            e.candidate != SidebarLayout::default()
                || *saved != SavedLayout::from(SidebarLayout::default())
        })
    }

    /// Undo is offered only with nothing unapplied, so it never discards a preview.
    pub fn can_undo(&self, saved: &SavedLayout) -> bool {
        self.replaced_layout.is_some() && !self.can_discard(saved)
    }

    pub fn dispatch(&mut self, action: Action, saved: &SavedLayout) -> Vec<Effect> {
        let mut effects = Vec::new();
        if action == Action::Open {
            if self.editing.is_none() {
                self.editing = Some(Editing {
                    candidate: saved.shown(),
                    revision: 0,
                    interpretation: Interpretation::Idle,
                    note: (SavedLayout::from(saved.shown()) != *saved).then(|| {
                        "The saved layout can't be read, so the default is shown. Apply replaces it.".into()
                    }),
                });
            }
            return effects;
        }
        let (can_apply, can_undo) = (self.can_apply(saved), self.can_undo(saved));
        let Some(editing) = &mut self.editing else {
            return effects;
        };
        if let Interpretation::Running { request, .. } = editing.interpretation {
            effects.push(Effect::Cancel(request));
        }
        editing.interpretation = Interpretation::Idle;
        let edit = |editing: &mut Editing, candidate: SidebarLayout| {
            editing.candidate = candidate;
            editing.revision += 1;
            editing.note = None;
        };
        match action {
            Action::Open => unreachable!(),
            Action::SetSide(side) => edit(
                editing,
                SidebarLayout {
                    side,
                    ..editing.candidate
                },
            ),
            Action::Resize(width) => edit(
                editing,
                SidebarLayout {
                    width,
                    ..editing.candidate
                },
            ),
            Action::Reset => {
                edit(editing, SidebarLayout::default());
                editing.note = Some("Default layout shown. Apply saves it.".into());
            }
            Action::Ask { provider, prompt } => {
                let prompt = prompt.trim().to_owned();
                if prompt.is_empty() {
                    editing.note = Some("Describe the change first.".into());
                } else {
                    self.last_ask = Some((provider, prompt));
                    effects.extend(self.start_request());
                }
            }
            Action::Retry => {
                if self.last_ask.is_some() {
                    effects.extend(self.start_request());
                }
            }
            Action::Cancel => editing.note = Some("Request cancelled.".into()),
            Action::Apply => {
                if can_apply {
                    effects.push(Effect::Save(editing.candidate.into(), Commit::Apply));
                } else {
                    editing.note = Some("Nothing to apply.".into());
                }
            }
            Action::Undo => {
                if can_undo && let Some(previous) = &self.replaced_layout {
                    effects.push(Effect::Save(previous.clone(), Commit::Undo));
                }
            }
            Action::Discard => {
                edit(editing, saved.shown());
                editing.note = Some("Preview discarded.".into());
            }
            Action::Close => self.editing = None,
        }
        effects
    }

    fn start_request(&mut self) -> Option<Effect> {
        let editing = self.editing.as_mut()?;
        let (provider, prompt) = self.last_ask.clone()?;
        self.next_request += 1;
        let request = self.next_request;
        editing.interpretation = Interpretation::Running {
            request,
            revision: editing.revision,
        };
        editing.note = None;
        Some(Effect::Ask(Ask {
            request,
            provider,
            prompt,
            current: editing.candidate,
        }))
    }

    /// A provider's answer. It counts only while the panel is open and the request is
    /// the one running for the current candidate; anything else is stale.
    pub fn receive(&mut self, request: u64, outcome: Outcome) {
        let Some(editing) = &mut self.editing else {
            return;
        };
        let Interpretation::Running {
            request: running,
            revision,
        } = editing.interpretation
        else {
            return;
        };
        if running != request || revision != editing.revision {
            return;
        }
        let provider = self
            .last_ask
            .as_ref()
            .map_or("The provider", |(p, _)| provider_name(*p));
        editing.interpretation = Interpretation::Idle;
        match outcome {
            Outcome::Proposal(Proposal::Change(layout)) => {
                editing.candidate = layout;
                editing.revision += 1;
                editing.note = Some(format!(
                    "{provider} proposed {} at {} px. Apply to keep it.",
                    layout.side.label(),
                    layout.width.px()
                ));
            }
            Outcome::Proposal(Proposal::Unsupported(reason)) => {
                editing.interpretation = Interpretation::Unsupported(reason)
            }
            Outcome::Failed(problem) => editing.interpretation = Interpretation::Failed(problem),
        }
    }

    /// The outcome of an [`Effect::Save`]. `previous` is what was saved before it.
    /// `Ok` carries a durability warning, if any: the layout was still applied.
    pub fn saved(
        &mut self,
        commit: Commit,
        previous: SavedLayout,
        layout: SavedLayout,
        result: Result<Option<String>, String>,
    ) {
        let Some(editing) = &mut self.editing else {
            return;
        };
        match result {
            Ok(warning) => {
                self.replaced_layout = Some(previous);
                editing.candidate = layout.shown();
                editing.revision += 1;
                let done = match commit {
                    Commit::Apply => "Applied.",
                    Commit::Undo => "Undone.",
                };
                editing.note = Some(match warning {
                    Some(warning) => format!("{done} {warning}"),
                    None => done.into(),
                });
            }
            Err(error) => editing.note = Some(format!("Not saved. {error}")),
        }
    }
}

struct DragHandle;

pub struct SidebarDrag {
    x: gpui::Pixels,
    width: SidebarWidth,
}

impl Keystone {
    pub(crate) fn saved_layout(&self) -> &SavedLayout {
        self.store.workspace().preferences().sidebar_layout()
    }

    /// Runs a customization action and its effects. Opening reveals a hidden sidebar
    /// and closing hides it again.
    pub(crate) fn customize(
        &mut self,
        action: Action,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let saved = self.saved_layout().clone();
        let (opening, closing) = (
            action == Action::Open && !self.customization.is_open(),
            action == Action::Close && self.customization.is_open(),
        );
        for effect in self.customization.dispatch(action, &saved) {
            match effect {
                Effect::Ask(ask) => self.customize_agent().ask(ask),
                Effect::Cancel(request) => {
                    if let Some(agent) = &self.customize_agent {
                        agent.cancel(request);
                    }
                }
                Effect::Save(layout, commit) => {
                    let result = match self.store.set_sidebar_layout(layout.clone()) {
                        Ok(applied) => {
                            let warning = durability_warning(applied.durability);
                            self.error = warning.clone();
                            Ok(warning)
                        }
                        Err(error) => Err(error.to_string()),
                    };
                    self.customization
                        .saved(commit, saved.clone(), layout, result);
                }
            }
        }
        if opening {
            self.sidebar_before_customize = Some(self.sidebar);
            self.sidebar = true;
            self.customize_prompt.focus_handle(cx).focus(window, cx);
        }
        if closing {
            if let Some(shown) = self.sidebar_before_customize.take() {
                self.sidebar = shown;
            }
            self.focus.focus(window, cx);
        }
        self.refresh(cx);
    }

    fn customize_agent(&mut self) -> &Agent {
        let wake = self.wake.clone();
        self.customize_agent.get_or_insert_with(|| {
            let programs = Programs {
                claude: std::env::var_os("KEYSTONE_CLAUDE_PATH").map(PathBuf::from),
                codex: std::env::var_os("KEYSTONE_CODEX_PATH").map(PathBuf::from),
            };
            Agent::new(programs, REQUEST_TIMEOUT, wake)
        })
    }

    pub(crate) fn receive_customization(&mut self) -> bool {
        let Some(agent) = &self.customize_agent else {
            return false;
        };
        let results = agent.drain();
        let any = !results.is_empty();
        for (request, outcome) in results {
            self.customization.receive(request, outcome);
        }
        any
    }

    pub(crate) fn customize_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let open = self.customization.is_open();
        Button::new("customize-toggle", "Customize")
            .size(ButtonSize::Compact)
            .selected(open)
            .debug_selector(|| "customize-toggle".into())
            .tooltip(Tooltip::text(if open {
                "Close customization and discard the preview"
            } else {
                "Customize the sidebar"
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                let action = if open { Action::Close } else { Action::Open };
                this.customize(action, window, cx)
            }))
    }

    fn step_width(&mut self, delta: f32, window: &mut Window, cx: &mut Context<Self>) {
        let width = self.customization.shown(self.saved_layout()).width.px();
        let width = SidebarWidth::clamped(f32::from(width) + delta);
        self.customize(Action::Resize(width), window, cx)
    }

    pub(crate) fn resize_handle(
        &self,
        side: SidebarSide,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors;
        div()
            .id("sidebar-resize-handle")
            .automation_id("sidebar-resize-handle")
            .absolute()
            .top_0()
            .bottom_0()
            .w(px(6.))
            .map(|d| match side {
                SidebarSide::Left => d.right_0(),
                SidebarSide::Right => d.left_0(),
            })
            .occlude()
            .cursor_col_resize()
            .bg(colors.border_focused.opacity(0.35))
            .hover(move |d| d.bg(colors.border_focused))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, _| {
                    let width = this.customization.shown(this.saved_layout()).width;
                    this.sidebar_drag = Some(SidebarDrag {
                        x: event.position.x,
                        width,
                    });
                }),
            )
            .on_drag(DragHandle, |_, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| Empty)
            })
    }

    pub(crate) fn on_sidebar_drag<E: InteractiveElement>(
        &self,
        root: E,
        cx: &mut Context<Self>,
    ) -> E {
        root.on_drag_move::<DragHandle>(cx.listener(
            |this, event: &gpui::DragMoveEvent<DragHandle>, window, cx| {
                let Some(start) = &this.sidebar_drag else {
                    return;
                };
                let moved = f32::from(event.event.position.x - start.x);
                let layout = this.customization.shown(this.saved_layout());
                let moved = match layout.side {
                    SidebarSide::Left => moved,
                    SidebarSide::Right => -moved,
                };
                let width = SidebarWidth::clamped(f32::from(start.width.px()) + moved);
                if layout.width != width {
                    this.customize(Action::Resize(width), window, cx);
                }
            },
        ))
    }

    pub(crate) fn customize_panel(
        &self,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors;
        let saved = self.saved_layout();
        let layout = self.customization.shown(saved);
        let drawn = layout.fit(f32::from(viewport.width)).round();
        let max_height = (f32::from(viewport.height) - PANEL_TOP - PANEL_MARGIN).max(0.);
        let running = matches!(
            self.customization.interpretation(),
            Some(Interpretation::Running { .. })
        );
        let action = |action: Action| {
            cx.listener(move |this: &mut Keystone, _, window, cx| {
                this.customize(action.clone(), window, cx)
            })
        };
        let row = |label: &'static str| {
            h_flex().gap_2().child(
                div()
                    .w(px(52.))
                    .child(Label::new(label).size(TextSize::Ui).color(Color::Muted)),
            )
        };
        let side = |value: SidebarSide, id: &'static str, label: &'static str| {
            Button::new(id, label)
                .size(ButtonSize::Compact)
                .style(ButtonStyle::Subtle)
                .selected(layout.side == value)
                .debug_selector(|| id.into())
                .on_click(action(Action::SetSide(value)))
        };
        let provider = self.customize_provider;
        let provider_button = |value: Provider, id: &'static str| {
            Button::new(id, provider_name(value))
                .size(ButtonSize::Compact)
                .style(ButtonStyle::Subtle)
                .selected(provider == value)
                .disabled(running)
                .debug_selector(|| id.into())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.customize_provider = value;
                    cx.notify();
                }))
        };
        let width_note = if drawn < f32::from(layout.width.px()) {
            format!("{} px · {drawn} px fits this window", layout.width.px())
        } else {
            format!("{} px", layout.width.px())
        };
        let (status, status_color) = match self.customization.interpretation() {
            Some(Interpretation::Running { .. }) => (
                Some(format!(
                    "Asking {}…",
                    provider_name(self.customization.asking().unwrap_or(provider))
                )),
                Color::Muted,
            ),
            Some(Interpretation::Failed(problem)) => (Some(problem.clone()), Color::Danger),
            Some(Interpretation::Unsupported(reason)) => {
                (Some(format!("Not supported: {reason}")), Color::Warning)
            }
            _ => (self.customization.note().map(str::to_owned), Color::Muted),
        };
        v_flex()
            .id("customize-panel")
            .automation_id("customize-panel")
            .key_context("Customize")
            .absolute()
            .top(px(PANEL_TOP))
            .map(|d| match layout.side {
                SidebarSide::Left => d.right(px(PANEL_MARGIN)),
                SidebarSide::Right => d.left(px(PANEL_MARGIN)),
            })
            .w(px(PANEL_WIDTH))
            .max_w_full()
            .max_h(px(max_height))
            .occlude()
            .surface(Elevation::Overlay, cx)
            .p(px(14.))
            .gap(px(10.))
            .on_action(cx.listener(|this, _: &SendMessage, window, cx| {
                let prompt = this.customize_prompt.read(cx).value().to_owned();
                let provider = this.customize_provider;
                this.customize(Action::Ask { provider, prompt }, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SideLeft, window, cx| {
                this.customize(Action::SetSide(SidebarSide::Left), window, cx)
            }))
            .on_action(cx.listener(|this, _: &SideRight, window, cx| {
                this.customize(Action::SetSide(SidebarSide::Right), window, cx)
            }))
            .on_action(cx.listener(|this, _: &Narrower, window, cx| {
                this.step_width(-WIDTH_STEP, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &Wider, window, cx| this.step_width(WIDTH_STEP, window, cx)),
            )
            .on_action(cx.listener(|this, _: &ApplyLayout, window, cx| {
                this.customize(Action::Apply, window, cx)
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                h_flex()
                    .flex_none()
                    .child(
                        Label::new("Customize sidebar")
                            .weight(FontWeight::SEMIBOLD)
                            .color(Color::Strong),
                    )
                    .child(div().flex_1())
                    .child(
                        IconButton::new("customize-close", IconName::Close)
                            .size(ButtonSize::Compact)
                            .debug_selector(|| "customize-close".into())
                            .tooltip(Tooltip::with_key("Close and discard the preview", "Escape"))
                            .on_click(action(Action::Close)),
                    ),
            )
            .child(
                v_flex()
                    .id("customize-body")
                    .min_h_0()
                    .overflow_y_scroll()
                    .gap(px(10.))
                    .child(
                        row("Side")
                            .child(side(SidebarSide::Left, "customize-side-left", "Left"))
                            .child(side(SidebarSide::Right, "customize-side-right", "Right")),
                    )
                    .child(
                        row("Width")
                            .child(
                                IconButton::new("customize-width-decrease", IconName::Minus)
                                    .size(ButtonSize::Compact)
                                    .style(ButtonStyle::Subtle)
                                    .disabled(layout.width.px() == SidebarWidth::MIN)
                                    .debug_selector(|| "customize-width-decrease".into())
                                    .tooltip(Tooltip::with_key("Narrower", "Alt+["))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.step_width(-WIDTH_STEP, window, cx)
                                    })),
                            )
                            .child(
                                div()
                                    .id("customize-width")
                                    .automation_id("customize-width")
                                    .child(Label::new(width_note).size(TextSize::Ui)),
                            )
                            .child(
                                IconButton::new("customize-width-increase", IconName::Plus)
                                    .size(ButtonSize::Compact)
                                    .style(ButtonStyle::Subtle)
                                    .disabled(layout.width.px() == SidebarWidth::MAX)
                                    .debug_selector(|| "customize-width-increase".into())
                                    .tooltip(Tooltip::with_key("Wider", "Alt+]"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.step_width(WIDTH_STEP, window, cx)
                                    })),
                            ),
                    )
                    .child(div().h_px().bg(colors.border))
                    .child(
                        row("Ask")
                            .child(provider_button(
                                Provider::Claude,
                                "customize-provider-claude",
                            ))
                            .child(provider_button(Provider::Codex, "customize-provider-codex")),
                    )
                    .child(
                        div()
                            .id("customize-prompt")
                            .automation_id("customize-prompt")
                            .px_2()
                            .rounded(radius::CONTROL)
                            .border_1()
                            .border_color(colors.border)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.customize_prompt.focus_handle(cx).focus(window, cx)
                            }))
                            .child(self.customize_prompt.clone()),
                    )
                    .child(
                        Label::new(
                            "Describe a sidebar change. Preview it here, then Apply to save.",
                        )
                        .size(TextSize::Sm)
                        .color(Color::Faint),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("customize-ask", "Preview")
                                    .size(ButtonSize::Compact)
                                    .style(ButtonStyle::Subtle)
                                    .debug_selector(|| "customize-ask".into())
                                    .tooltip(Tooltip::with_key("Ask for a preview", "Ctrl+Enter"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let prompt =
                                            this.customize_prompt.read(cx).value().to_owned();
                                        let provider = this.customize_provider;
                                        this.customize(Action::Ask { provider, prompt }, window, cx)
                                    })),
                            )
                            .when(running, |d| {
                                d.child(
                                    Button::new("customize-cancel", "Cancel")
                                        .size(ButtonSize::Compact)
                                        .debug_selector(|| "customize-cancel".into())
                                        .on_click(action(Action::Cancel)),
                                )
                            })
                            .when(self.customization.can_retry(), |d| {
                                d.child(
                                    Button::new("customize-retry", "Retry")
                                        .size(ButtonSize::Compact)
                                        .debug_selector(|| "customize-retry".into())
                                        .on_click(action(Action::Retry)),
                                )
                            }),
                    ),
            )
            .child(
                v_flex()
                    .flex_none()
                    .gap(px(10.))
                    .children(status.map(|status| {
                        div()
                            .id("customize-status")
                            .automation_id("customize-status")
                            .child(Label::new(status).size(TextSize::Ui).color(status_color))
                    }))
                    .child(div().h_px().bg(colors.border))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("customize-reset", "Reset")
                                    .size(ButtonSize::Compact)
                                    .disabled(!self.customization.can_reset(saved))
                                    .debug_selector(|| "customize-reset".into())
                                    .tooltip(Tooltip::text("Preview the default layout"))
                                    .on_click(action(Action::Reset)),
                            )
                            .child(div().flex_1())
                            .child(
                                Button::new("customize-undo", "Undo")
                                    .size(ButtonSize::Compact)
                                    .disabled(!self.customization.can_undo(saved))
                                    .debug_selector(|| "customize-undo".into())
                                    .tooltip(Tooltip::text(
                                        "Restore the layout before the last Apply",
                                    ))
                                    .on_click(action(Action::Undo)),
                            )
                            .child(
                                Button::new("customize-discard", "Discard")
                                    .size(ButtonSize::Compact)
                                    .disabled(!self.customization.can_discard(saved))
                                    .debug_selector(|| "customize-discard".into())
                                    .on_click(action(Action::Discard)),
                            )
                            .child(
                                Button::new("customize-apply", "Apply")
                                    .size(ButtonSize::Compact)
                                    .style(ButtonStyle::Filled)
                                    .disabled(!self.customization.can_apply(saved))
                                    .debug_selector(|| "customize-apply".into())
                                    .tooltip(Tooltip::with_key("Save this layout", "Ctrl+S"))
                                    .on_click(action(Action::Apply)),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(side: SidebarSide, width: u16) -> SidebarLayout {
        SidebarLayout {
            side,
            width: SidebarWidth::new(width).unwrap(),
        }
    }

    fn unreadable() -> SavedLayout {
        serde_json::from_str(r#"{"side":"top","width_px":360,"pinned":true}"#).unwrap()
    }

    fn ask(c: &mut Customization, saved: &SavedLayout) -> u64 {
        let effects = c.dispatch(
            Action::Ask {
                provider: Provider::Claude,
                prompt: "right and wider".into(),
            },
            saved,
        );
        match effects.as_slice() {
            [Effect::Ask(ask)] | [Effect::Cancel(_), Effect::Ask(ask)] => ask.request,
            other => panic!("{other:?}"),
        }
    }

    fn change(side: SidebarSide, width: u16) -> Outcome {
        Outcome::Proposal(Proposal::Change(layout(side, width)))
    }

    #[test]
    fn a_manual_edit_cancels_a_request_and_its_late_answer_is_ignored() {
        let saved = SavedLayout::default();
        let mut c = Customization::default();
        c.dispatch(Action::Open, &saved);
        let request = ask(&mut c, &saved);
        let effects = c.dispatch(Action::Resize(SidebarWidth::new(400).unwrap()), &saved);
        assert_eq!(effects, vec![Effect::Cancel(request)]);
        c.receive(request, change(SidebarSide::Right, 360));
        assert_eq!(c.shown(&saved), layout(SidebarSide::Left, 400));
        assert_eq!(c.interpretation(), Some(&Interpretation::Idle));
    }

    #[test]
    fn only_the_current_request_for_the_current_candidate_is_accepted() {
        let saved = SavedLayout::default();
        let mut c = Customization::default();
        c.receive(1, change(SidebarSide::Right, 360));
        assert_eq!(c.shown(&saved), saved.shown());

        c.dispatch(Action::Open, &saved);
        let first = ask(&mut c, &saved);
        let second = ask(&mut c, &saved);
        c.receive(first, change(SidebarSide::Right, 500));
        assert_eq!(c.shown(&saved), saved.shown());
        c.receive(second, change(SidebarSide::Right, 360));
        assert_eq!(c.shown(&saved), layout(SidebarSide::Right, 360));
        c.receive(second, change(SidebarSide::Left, 220));
        assert_eq!(c.shown(&saved), layout(SidebarSide::Right, 360));
    }

    #[test]
    fn cancel_keeps_the_candidate_and_close_discards_it() {
        let saved = SavedLayout::default();
        let mut c = Customization::default();
        c.dispatch(Action::Open, &saved);
        c.dispatch(Action::SetSide(SidebarSide::Right), &saved);
        let request = ask(&mut c, &saved);
        assert_eq!(
            c.dispatch(Action::Cancel, &saved),
            vec![Effect::Cancel(request)]
        );
        assert_eq!(c.shown(&saved), layout(SidebarSide::Right, 302));
        c.receive(request, change(SidebarSide::Left, 500));
        assert_eq!(c.shown(&saved), layout(SidebarSide::Right, 302));

        let request = ask(&mut c, &saved);
        assert_eq!(
            c.dispatch(Action::Close, &saved),
            vec![Effect::Cancel(request)]
        );
        assert!(!c.is_open());
        assert_eq!(c.shown(&saved), saved.shown());
    }

    #[test]
    fn a_running_request_is_cancelled_even_with_nothing_to_apply_or_discard() {
        let saved = SavedLayout::default();
        let mut c = Customization::default();
        c.dispatch(Action::Open, &saved);
        let request = ask(&mut c, &saved);
        assert!(!c.can_apply(&saved) && !c.can_discard(&saved));
        assert_eq!(
            c.dispatch(Action::Cancel, &saved),
            vec![Effect::Cancel(request)]
        );
        assert_eq!(c.note(), Some("Request cancelled."));
        let request = ask(&mut c, &saved);
        assert_eq!(
            c.dispatch(Action::Apply, &saved),
            vec![Effect::Cancel(request)]
        );
        assert_eq!(c.note(), Some("Nothing to apply."));
    }

    #[test]
    fn failures_and_unsupported_answers_keep_the_candidate_and_allow_retry() {
        let saved = SavedLayout::default();
        let mut c = Customization::default();
        c.dispatch(Action::Open, &saved);
        c.dispatch(Action::SetSide(SidebarSide::Right), &saved);
        let request = ask(&mut c, &saved);
        c.receive(
            request,
            Outcome::Failed("Claude Code is not installed.".into()),
        );
        assert!(c.can_retry());
        assert_eq!(c.shown(&saved), layout(SidebarSide::Right, 302));
        let effects = c.dispatch(Action::Retry, &saved);
        let [Effect::Ask(retry)] = effects.as_slice() else {
            panic!("{effects:?}")
        };
        assert_eq!(retry.prompt, "right and wider");
        assert_eq!(retry.current, layout(SidebarSide::Right, 302));
        c.receive(
            retry.request,
            Outcome::Proposal(Proposal::Unsupported("No colors.".into())),
        );
        assert_eq!(
            c.interpretation(),
            Some(&Interpretation::Unsupported("No colors.".into()))
        );
        assert_eq!(c.shown(&saved), layout(SidebarSide::Right, 302));
    }

    #[test]
    fn apply_and_undo_change_history_only_after_a_successful_save() {
        let original = SavedLayout::default();
        let right = layout(SidebarSide::Right, 360);
        let mut c = Customization::default();
        c.dispatch(Action::Open, &original);
        assert!(!c.can_apply(&original) && !c.can_reset(&original));
        c.dispatch(Action::SetSide(SidebarSide::Right), &original);
        c.dispatch(Action::Resize(right.width), &original);
        assert!(!c.can_undo(&original));
        assert!(c.can_apply(&original) && c.can_discard(&original));

        let effects = c.dispatch(Action::Apply, &original);
        assert_eq!(effects, vec![Effect::Save(right.into(), Commit::Apply)]);
        c.saved(
            Commit::Apply,
            original.clone(),
            right.into(),
            Err("disk full".into()),
        );
        assert_eq!(c.shown(&original), right);
        assert!(!c.can_undo(&original));
        assert_eq!(c.note(), Some("Not saved. disk full"));

        c.saved(Commit::Apply, original.clone(), right.into(), Ok(None));
        let saved = SavedLayout::from(right);
        assert!(c.can_undo(&saved));
        assert!(!c.can_apply(&saved) && !c.can_discard(&saved));
        assert_eq!(c.dispatch(Action::Apply, &saved), vec![]);
        assert!(c.can_undo(&saved));

        c.dispatch(Action::Resize(SidebarWidth::new(500).unwrap()), &saved);
        assert!(!c.can_undo(&saved));
        assert_eq!(c.dispatch(Action::Undo, &saved), vec![]);
        c.dispatch(Action::Discard, &saved);
        assert!(c.can_undo(&saved));

        assert_eq!(
            c.dispatch(Action::Undo, &saved),
            vec![Effect::Save(original.clone(), Commit::Undo)]
        );
        c.saved(Commit::Undo, saved.clone(), original.clone(), Ok(None));
        assert_eq!(c.shown(&original), original.shown());
        assert_eq!(
            c.dispatch(Action::Undo, &original),
            vec![Effect::Save(right.into(), Commit::Undo)]
        );
    }

    #[test]
    fn an_unreadable_layout_can_be_replaced_by_the_default_and_undone_exactly() {
        let unreadable = unreadable();
        let default = SavedLayout::from(SidebarLayout::default());
        let mut c = Customization::default();
        c.dispatch(Action::Open, &unreadable);
        assert_eq!(c.shown(&unreadable), SidebarLayout::default());
        assert!(c.can_apply(&unreadable) && c.can_reset(&unreadable));
        assert!(!c.can_discard(&unreadable) && !c.can_undo(&unreadable));
        assert!(c.note().unwrap().contains("can't be read"));

        assert_eq!(c.dispatch(Action::Reset, &unreadable), vec![]);
        assert!(c.can_apply(&unreadable) && c.can_reset(&unreadable));
        assert_eq!(
            c.dispatch(Action::Apply, &unreadable),
            vec![Effect::Save(default.clone(), Commit::Apply)]
        );
        c.saved(
            Commit::Apply,
            unreadable.clone(),
            default.clone(),
            Err("disk full".into()),
        );
        assert!(c.can_apply(&unreadable) && !c.can_undo(&unreadable));
        assert_eq!(c.note(), Some("Not saved. disk full"));

        c.saved(Commit::Apply, unreadable.clone(), default.clone(), Ok(None));
        assert!(!c.can_apply(&default) && !c.can_reset(&default));
        assert!(c.can_undo(&default));
        assert_eq!(c.dispatch(Action::Apply, &default), vec![]);
        assert_eq!(c.note(), Some("Nothing to apply."));

        assert_eq!(
            c.dispatch(Action::Undo, &default),
            vec![Effect::Save(unreadable.clone(), Commit::Undo)]
        );
        c.saved(Commit::Undo, default.clone(), unreadable.clone(), Ok(None));
        assert_eq!(c.note(), Some("Undone."));
        assert_eq!(c.shown(&unreadable), SidebarLayout::default());
        assert!(c.can_apply(&unreadable) && c.can_reset(&unreadable));
        assert!(c.can_undo(&unreadable) && !c.can_discard(&unreadable));
        assert_eq!(
            c.dispatch(Action::Undo, &unreadable),
            vec![Effect::Save(default, Commit::Undo)]
        );
    }

    #[test]
    fn a_pending_directory_sync_still_counts_as_applied() {
        let original = SavedLayout::default();
        let right = SavedLayout::from(layout(SidebarSide::Right, 302));
        let mut c = Customization::default();
        c.dispatch(Action::Open, &original);
        c.dispatch(Action::SetSide(SidebarSide::Right), &original);
        c.saved(
            Commit::Apply,
            original,
            right.clone(),
            Ok(Some(
                "Change saved, but crash durability is unconfirmed.".into(),
            )),
        );
        assert!(c.can_undo(&right));
        assert!(c.note().unwrap().starts_with("Applied. Change saved"));
    }

    #[test]
    fn reset_previews_the_default_until_applied() {
        let saved = SavedLayout::from(layout(SidebarSide::Right, 480));
        let mut c = Customization::default();
        c.dispatch(Action::Open, &saved);
        assert!(c.can_reset(&saved));
        assert_eq!(c.dispatch(Action::Reset, &saved), vec![]);
        assert_eq!(c.shown(&saved), SidebarLayout::default());
        assert!(c.can_reset(&saved) && c.can_apply(&saved));
        assert_eq!(
            c.dispatch(Action::Apply, &saved),
            vec![Effect::Save(SidebarLayout::default().into(), Commit::Apply)]
        );
    }

    #[test]
    fn manual_edits_clear_a_provider_error_before_saving() {
        let saved = SavedLayout::default();
        let mut c = Customization::default();
        c.dispatch(Action::Open, &saved);
        let request = ask(&mut c, &saved);
        c.receive(request, Outcome::Failed("Provider unavailable".into()));
        assert!(c.can_retry());
        c.dispatch(Action::SetSide(SidebarSide::Right), &saved);
        assert_eq!(c.interpretation(), Some(&Interpretation::Idle));
        assert!(!c.can_retry());
        let next = c.shown(&saved);
        c.saved(Commit::Apply, saved, next.into(), Ok(None));
        assert_eq!(c.note(), Some("Applied."));
    }
}

#[cfg(all(test, feature = "ui-tests"))]
mod ui_tests {
    use super::*;
    use crate::customization_agent::Programs;
    use gpui::{Entity, Modifiers, Pixels, Point, TestAppContext, VisualTestContext, point};
    use keystone_core::Store;
    use std::path::Path;
    use std::time::Duration;

    fn open<'a>(
        path: &Path,
        cx: &'a mut TestAppContext,
    ) -> (Entity<Keystone>, &'a mut VisualTestContext) {
        let store = Store::open(path).unwrap();
        cx.update(|cx| {
            keystone_ui::init(cx);
            crate::bind_keys(cx);
        });
        cx.executor().allow_parking();
        cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx))
    }

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} not rendered"));
        cx.simulate_click(bounds.center(), Modifiers::none());
        cx.run_until_parked();
    }

    fn drag(cx: &mut VisualTestContext, from: Point<Pixels>, by: f32) {
        let to = point(from.x + px(by), from.y);
        cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(
            point(from.x + px(by / 2.), from.y),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
    }

    fn saved(path: &Path) -> SidebarLayout {
        Store::open(path)
            .unwrap()
            .workspace()
            .preferences()
            .sidebar_layout()
            .shown()
    }

    fn saved_json(path: &Path) -> serde_json::Value {
        let state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        state["workspace"]["preferences"].clone()
    }

    fn shown(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> SidebarLayout {
        view.read_with(cx, |view, _| view.customization.shown(view.saved_layout()))
    }

    fn layout(side: SidebarSide, width: u16) -> SidebarLayout {
        SidebarLayout {
            side,
            width: SidebarWidth::new(width).unwrap(),
        }
    }

    fn search_x(cx: &mut VisualTestContext) -> f32 {
        f32::from(cx.debug_bounds("search-field").unwrap().origin.x)
    }

    #[gpui::test]
    fn escape_closes_the_rename_dialog_before_customization(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = open(&dir.path().join("workspace.json"), cx);
        click(cx, "welcome-codex");
        click(cx, "customize-toggle");
        cx.simulate_keystrokes("f2");
        cx.run_until_parked();
        assert!(cx.debug_bounds("rename-dialog").is_some());
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(cx.debug_bounds("rename-dialog").is_none());
        assert!(cx.debug_bounds("customize-panel").is_some());
        assert_eq!(shown(&view, cx), SidebarLayout::default());
    }

    #[gpui::test]
    fn direct_edits_preview_apply_undo_and_restore_a_hidden_sidebar(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let (view, cx) = open(&path, cx);
        click(cx, "sidebar-toggle");
        assert!(cx.debug_bounds("search-field").is_none());

        click(cx, "customize-toggle");
        assert!(cx.debug_bounds("customize-panel").is_some());
        assert!(search_x(cx) < 50.);
        view.update_in(cx, |view, window, cx| {
            assert!(view.customize_prompt.focus_handle(cx).is_focused(window))
        });

        click(cx, "customize-side-right");
        click(cx, "customize-width-increase");
        click(cx, "customize-width-increase");
        assert_eq!(shown(&view, cx), layout(SidebarSide::Right, 322));
        assert!(search_x(cx) > 300., "the sidebar moved right");
        assert_eq!(saved(&path), SidebarLayout::default());

        click(cx, "customize-apply");
        assert_eq!(saved(&path), layout(SidebarSide::Right, 322));
        click(cx, "customize-undo");
        assert_eq!(saved(&path), SidebarLayout::default());
        assert_eq!(shown(&view, cx), SidebarLayout::default());
        click(cx, "customize-undo");
        assert_eq!(saved(&path), layout(SidebarSide::Right, 322));

        click(cx, "customize-reset");
        assert_eq!(shown(&view, cx), SidebarLayout::default());
        assert_eq!(saved(&path), layout(SidebarSide::Right, 322));
        click(cx, "customize-discard");
        assert_eq!(shown(&view, cx), layout(SidebarSide::Right, 322));

        click(cx, "customize-side-left");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(cx.debug_bounds("customize-panel").is_none());
        assert!(cx.debug_bounds("search-field").is_none());
        assert_eq!(shown(&view, cx), layout(SidebarSide::Right, 322));

        click(cx, "sidebar-toggle");
        assert!(search_x(cx) > 300.);
    }

    #[gpui::test]
    fn an_unreadable_saved_layout_is_replaced_by_apply_and_restored_by_undo(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let raw = serde_json::json!({"side": "top", "width_px": 360, "pinned": true});
        std::fs::write(
            &path,
            serde_json::json!({
                "version": 1,
                "next_id": 2,
                "future": {"a": [1, 2]},
                "workspace": {
                    "sessions": [{"id": 1, "title": "Old", "provider": "Codex", "settled": false, "draft": "keep", "model": "m-1"}],
                    "selected": 1,
                    "preferences": {"theme": "Dark", "accent": "teal", "sidebar_layout": raw},
                },
            })
            .to_string(),
        )
        .unwrap();
        let (view, cx) = open(&path, cx);
        assert_eq!(shown(&view, cx), SidebarLayout::default());

        click(cx, "customize-toggle");
        view.read_with(cx, |view, _| {
            assert!(view.customization.note().unwrap().contains("can't be read"))
        });
        click(cx, "customize-reset");
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.customization.note(),
                Some("Default layout shown. Apply saves it.")
            )
        });
        assert_eq!(saved_json(&path)["sidebar_layout"], raw);

        click(cx, "customize-apply");
        view.read_with(cx, |view, _| {
            assert_eq!(view.customization.note(), Some("Applied."))
        });
        let preferences = saved_json(&path);
        assert_eq!(
            preferences["sidebar_layout"],
            serde_json::json!({"side": "left", "width_px": 302})
        );
        assert_eq!(preferences["accent"], "teal");

        for selector in ["settings", "theme-light"] {
            click(cx, selector);
        }
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(saved_json(&path)["theme"], "Light");

        click(cx, "customize-undo");
        view.read_with(cx, |view, _| {
            assert_eq!(view.customization.note(), Some("Undone."))
        });
        let preferences = saved_json(&path);
        assert_eq!(preferences["sidebar_layout"], raw);
        assert_eq!(preferences["theme"], "Light");
        assert_eq!(preferences["accent"], "teal");
        assert_eq!(shown(&view, cx), SidebarLayout::default());

        for selector in ["settings", "density-compact"] {
            click(cx, selector);
        }
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        let preferences = saved_json(&path);
        assert_eq!(preferences["sidebar_layout"], raw);
        assert_eq!(preferences["density"], "Compact");
        let store = Store::open(&path).unwrap();
        assert_eq!(store.workspace().sessions()[0].draft(), "keep");
    }

    #[gpui::test]
    fn dragging_the_inner_edge_widens_on_either_side_and_a_small_window_narrows_it(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let (view, cx) = open(&path, cx);
        cx.simulate_resize(gpui::size(px(1100.), px(720.)));
        cx.run_until_parked();
        assert!(cx.debug_bounds("sidebar-resize-handle").is_none());
        click(cx, "customize-toggle");

        let handle = cx.debug_bounds("sidebar-resize-handle").unwrap();
        drag(cx, handle.center(), 40.);
        assert_eq!(shown(&view, cx).width.px(), 342);

        click(cx, "customize-side-right");
        let handle = cx.debug_bounds("sidebar-resize-handle").unwrap();
        drag(cx, handle.center(), -60.);
        assert_eq!(shown(&view, cx).width.px(), 402);
        let handle = cx.debug_bounds("sidebar-resize-handle").unwrap();
        drag(cx, handle.center(), 600.);
        assert_eq!(shown(&view, cx).width.px(), SidebarWidth::MIN);
        click(cx, "customize-width-increase");
        click(cx, "customize-apply");
        assert_eq!(saved(&path), layout(SidebarSide::Right, 230));

        click(cx, "customize-width-increase");
        click(cx, "customize-width-increase");
        click(cx, "customize-apply");
        let window_width = 560.;
        cx.simulate_resize(gpui::size(px(window_width), px(720.)));
        cx.run_until_parked();
        let handle = cx.debug_bounds("sidebar-resize-handle").unwrap();
        let fitted = layout(SidebarSide::Right, 250).fit(window_width);
        assert_eq!(
            handle.origin.x,
            px(window_width - fitted + keystone_core::DIVIDER_WIDTH)
        );
        assert_eq!(saved(&path), layout(SidebarSide::Right, 250));
        let width = cx.debug_bounds("customize-width").unwrap();
        assert!(width.size.width > px(0.));
    }

    #[gpui::test]
    fn a_short_window_keeps_the_status_and_commit_buttons_inside_it(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let (_view, cx) = open(&path, cx);
        let height = px(360.);
        cx.simulate_resize(gpui::size(px(1100.), height));
        cx.run_until_parked();
        click(cx, "customize-toggle");
        click(cx, "customize-side-right");
        cx.simulate_keystrokes("ctrl-enter");
        cx.run_until_parked();

        let panel = cx.debug_bounds("customize-panel").unwrap();
        assert!(panel.bottom() <= height, "{panel:?}");
        for selector in [
            "customize-status",
            "customize-reset",
            "customize-undo",
            "customize-discard",
            "customize-apply",
        ] {
            let bounds = cx.debug_bounds(selector).unwrap();
            assert!(
                bounds.origin.y >= panel.origin.y && bounds.bottom() <= panel.bottom(),
                "{selector} {bounds:?} is outside {panel:?}"
            );
        }
        click(cx, "customize-apply");
        assert_eq!(saved(&path).side, SidebarSide::Right);
    }

    #[gpui::test]
    fn ctrl_enter_in_the_prompt_asks_the_provider_and_leaves_the_conversation_alone(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let reply = r#"{"kind":"change","version":1,"side":"right","width_px":360}"#;
        let claude = crate::customization_agent::tests_support::wrapper(
            dir.path(),
            "keystone-fake-claude",
            &format!("KEYSTONE_FAKE_CLAUDE_REPLY='{reply}' KEYSTONE_FAKE_CLAUDE_DELAY_MS=1"),
        );
        let (view, cx) = open(&path, cx);
        click(cx, "welcome-codex");
        cx.simulate_input("keep this draft");
        view.update(cx, |view, _| {
            let wake = view.wake.clone();
            view.customize_agent = Some(Agent::new(
                Programs {
                    claude: Some(claude),
                    codex: None,
                },
                Duration::from_secs(20),
                wake,
            ));
        });

        click(cx, "customize-toggle");
        cx.simulate_input("move it right and make it wider");
        cx.simulate_keystrokes("ctrl-enter");
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while shown(&view, cx) != layout(SidebarSide::Right, 360) {
            assert!(std::time::Instant::now() < deadline, "no proposal arrived");
            std::thread::sleep(Duration::from_millis(10));
            cx.run_until_parked();
        }
        assert_eq!(saved(&path), SidebarLayout::default());
        view.read_with(cx, |view, cx| {
            assert_eq!(view.composer.read(cx).value(), "keep this draft");
            assert_eq!(
                view.customize_prompt.read(cx).value(),
                "move it right and make it wider"
            );
            assert!(
                view.conversations
                    .get(&1)
                    .is_none_or(|c| !c.running && c.is_empty())
            );
        });
        click(cx, "customize-apply");
        assert_eq!(saved(&path), layout(SidebarSide::Right, 360));
        cx.executor().advance_clock(Duration::from_millis(300));
        cx.run_until_parked();
        let store = Store::open(&path).unwrap();
        assert_eq!(store.workspace().sessions()[0].draft(), "keep this draft");
    }
}
