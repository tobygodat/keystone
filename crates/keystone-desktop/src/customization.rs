use crate::arrangement_editor::{Action, Chooser, Effect, Interpretation};
use crate::customization_agent::{Agent, Outcome, Programs, REQUEST_TIMEOUT};
use crate::{Keystone, SendMessage};
use gpui::{
    App, Context, Focusable, FontWeight, IntoElement, MouseButton, SharedString, Window, div,
    prelude::*, px,
};
use keystone_core::{
    Axis, Component, Composition, Edit, Node, NodePath, Place, Provider, SavedComposition, Size,
};
use keystone_ui::prelude::*;
use std::num::NonZeroU16;

gpui::actions!(
    customize,
    [
        OpenCustomize,
        MoveLeft,
        MoveRight,
        MoveUp,
        MoveDown,
        Narrower,
        Wider,
        ApplyLayout
    ]
);
pub fn bind_keys(cx: &mut gpui::App) {
    cx.bind_keys([
        gpui::KeyBinding::new(
            "alt-left",
            MoveLeft,
            Some("Customize && !Dialog && !CommandPalette"),
        ),
        gpui::KeyBinding::new(
            "alt-right",
            MoveRight,
            Some("Customize && !Dialog && !CommandPalette"),
        ),
        gpui::KeyBinding::new(
            "alt-up",
            MoveUp,
            Some("Customize && !Dialog && !CommandPalette"),
        ),
        gpui::KeyBinding::new(
            "alt-down",
            MoveDown,
            Some("Customize && !Dialog && !CommandPalette"),
        ),
        gpui::KeyBinding::new(
            "alt-[",
            Narrower,
            Some("Customize && !Dialog && !CommandPalette"),
        ),
        gpui::KeyBinding::new(
            "alt-]",
            Wider,
            Some("Customize && !Dialog && !CommandPalette"),
        ),
        gpui::KeyBinding::new(
            "secondary-s",
            ApplyLayout,
            Some("Customize && !Dialog && !CommandPalette"),
        ),
    ]);
}

pub fn label(component: Component) -> &'static str {
    match component {
        Component::SessionList => "Session list",
        Component::Conversation => "Conversation",
        Component::Composer => "Composer",
        Component::Files => "Files",
        Component::Changes => "Changes",
        Component::Notes => "Notes",
        Component::PullRequest => "Pull request",
        Component::Browser => "Browser",
        Component::Device => "Device",
    }
}

impl Keystone {
    pub(crate) fn saved_layout<'a>(&self, cx: &'a App) -> Option<&'a SavedComposition> {
        self.presentation.project.as_ref().ok().and_then(|p| {
            self.state
                .read(cx)
                .store()
                .workspace()
                .preferences()
                .layout_in(p, self.window_id)
        })
    }
    pub(crate) fn saved_composition(&self, cx: &App) -> Composition {
        self.presentation.project.as_ref().map_or_else(
            |_| {
                Composition::from_sidebar(
                    self.state
                        .read(cx)
                        .store()
                        .workspace()
                        .preferences()
                        .sidebar_layout()
                        .shown(),
                )
            },
            |p| {
                self.state
                    .read(cx)
                    .store()
                    .workspace()
                    .preferences()
                    .composition_in(p, self.window_id)
            },
        )
    }
    pub(crate) fn customize(
        &mut self,
        action: Action,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let saved = self.saved_composition(cx);
        let raw = self.saved_layout(cx).cloned();
        let opening = action == Action::Open && !self.presentation.current.customization.is_open();
        let closing = action == Action::Close && self.presentation.current.customization.is_open();
        if opening || matches!(action, Action::Reset | Action::Discard | Action::Close) {
            self.presentation.current.arrangement.restore_tabs();
        }
        let effects =
            self.presentation
                .current
                .customization
                .dispatch(action, raw.as_ref(), &saved);
        self.customize_effects(effects, window, cx);
        if opening {
            self.presentation.current.sidebar_before_customize =
                Some(self.presentation.current.sidebar);
            self.presentation.current.sidebar = true;
            self.presentation
                .current
                .customize_prompt
                .focus_handle(cx)
                .focus(window, cx);
        }
        if closing {
            if let Some(shown) = self.presentation.current.sidebar_before_customize.take() {
                self.presentation.current.sidebar = shown;
            }
            self.focus.focus(window, cx);
        }
        if self.presentation.current.customization.chooser().is_some() {
            if self.presentation.current.borrow_focus.is_none() {
                self.presentation.current.borrow_focus = Some(cx.focus_handle());
            }
            self.presentation
                .current
                .borrow_focus
                .as_ref()
                .unwrap()
                .focus(window, cx);
        } else if self.presentation.current.borrow_focus.take().is_some() {
            self.presentation
                .current
                .customize_prompt
                .focus_handle(cx)
                .focus(window, cx);
        }
        self.refresh(cx);
    }
    fn customize_effects(
        &mut self,
        effects: Vec<Effect>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for effect in effects {
            match effect {
                Effect::Ask(ask) => self.customize_agent(cx).ask(ask),
                Effect::Cancel(request) => {
                    if let Some(agent) = &self.customize_agent {
                        agent.cancel(request);
                    }
                }
                Effect::Save(layout) => {
                    let previous = self.saved_layout(cx).cloned();
                    let window_id = self.window_id;
                    let result = match &self.presentation.project {
                        Ok(project) => self
                            .state
                            .update(cx, |state, cx| {
                                state.set_layout(project, window_id, layout, cx)
                            })
                            .map(|_| ())
                            .map_err(|e| e.to_string()),
                        Err(error) => Err(format!("Cannot save this project's layout. {error}")),
                    };
                    if result.is_ok() {
                        self.presentation.current.arrangement.restore_tabs();
                        self.presentation.current.arrangement.seen = None;
                    }
                    let shown = self.saved_composition(cx);
                    self.presentation
                        .current
                        .customization
                        .saved(previous, shown, result);
                }
                Effect::Borrow(source) => {
                    let raw = self
                        .state
                        .read(cx)
                        .store()
                        .workspace()
                        .preferences()
                        .layout_in(&source, self.window_id)
                        .cloned();
                    let effects = self
                        .presentation
                        .current
                        .customization
                        .borrowed(&source, raw.as_ref());
                    self.customize_effects(effects, window, cx);
                }
            }
        }
    }
    fn customize_agent(&mut self, cx: &App) -> &Agent {
        let wake = self.customize_wake.clone();
        // A test build never reads `KEYSTONE_CLAUDE_PATH` here: another test may have set
        // it, and without one the helper would run the `claude` on `PATH` (TOB-363).
        let claude = self.state.read(cx).provider_program(Provider::Claude);
        self.customize_agent
            .get_or_insert_with(|| Agent::new(Programs { claude }, REQUEST_TIMEOUT, wake))
    }
    pub(crate) fn receive_customization(&mut self) -> bool {
        let Some(agent) = &self.customize_agent else {
            return false;
        };
        let results = agent.drain();
        let any = !results.is_empty();
        for (request, outcome) in results {
            self.receive_customization_outcome(request, outcome);
        }
        any
    }
    /// Takes in the agent's answer to `request`; a proposal it previews replaces the
    /// fitted tab the user had chosen.
    pub(crate) fn receive_customization_outcome(&mut self, request: u64, outcome: Outcome) {
        if self
            .presentation
            .current
            .customization
            .receive(request, outcome)
        {
            self.presentation.current.arrangement.restore_tabs();
        }
    }
    pub(crate) fn customize_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let open = self.presentation.current.customization.is_open();
        Button::new("customize-toggle", "Customize")
            .size(ButtonSize::Compact)
            .selected(open)
            .debug_selector(|| "customize-toggle".into())
            .tooltip(Tooltip::text(crate::commands::hint("customize-layout", cx)))
            .on_click(self.scoped_listener(cx, move |this, _, window, cx| {
                this.customize(if open { Action::Close } else { Action::Open }, window, cx)
            }))
    }
    pub(crate) fn move_component(
        &mut self,
        axis: Axis,
        after: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.presentation.current.customization.selected() else {
            return;
        };
        let c = self
            .presentation
            .current
            .customization
            .shown(&self.saved_composition(cx))
            .clone();
        let order = crate::arrangement::ordered_views(c.root());
        let Some(at) = order.iter().position(|id| *id == view) else {
            return;
        };
        let target = if after {
            order.get(at + 1)
        } else {
            at.checked_sub(1).and_then(|i| order.get(i))
        };
        if let Some(&target) = target {
            self.customize(
                Action::Edit(Edit::Move {
                    view,
                    to: Place::Beside {
                        target,
                        axis,
                        after,
                    },
                }),
                window,
                cx,
            );
        }
    }
    pub(crate) fn size_component(
        &mut self,
        delta: i32,
        mode: Option<Size>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.presentation.current.customization.selected() else {
            return;
        };
        fn find(
            node: &Node,
            id: keystone_core::ViewId,
            path: &mut Vec<usize>,
            delta: i32,
            mode: Option<Size>,
        ) -> Option<(NodePath, Vec<Size>)> {
            if let Node::Split { children, .. } = node {
                for (i, child) in children.iter().enumerate() {
                    if crate::arrangement::ordered_views(&child.node).contains(&id) {
                        path.push(i);
                        if let Some(found) = find(&child.node, id, path, delta, mode) {
                            return Some(found);
                        }
                        path.pop();
                        let mut sizes: Vec<_> = children.iter().map(|c| c.size).collect();
                        sizes[i] =
                            mode.unwrap_or_else(|| match sizes[i] {
                                Size::Px(n) => Size::Px(
                                    NonZeroU16::new(
                                        (i32::from(n.get()) + 10 * delta).clamp(1, 65535) as u16,
                                    )
                                    .unwrap(),
                                ),
                                Size::Fill(n) => Size::Fill(
                                    NonZeroU16::new(
                                        (i32::from(n.get()) + delta).clamp(1, 65535) as u16
                                    )
                                    .unwrap(),
                                ),
                                Size::Auto => Size::Auto,
                            });
                        return Some((NodePath(path.clone()), sizes));
                    }
                }
            }
            None
        }
        let c = self
            .presentation
            .current
            .customization
            .shown(&self.saved_composition(cx))
            .clone();
        if let Some((split, sizes)) = find(c.root(), id, &mut vec![], delta, mode) {
            self.customize(Action::Edit(Edit::Resize { split, sizes }), window, cx);
        }
    }
    pub(crate) fn customize_panel(
        &self,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors;
        let saved = self.saved_composition(cx);
        let running = self.presentation.current.customization.running().is_some();
        let dirty = self.presentation.current.customization.can_discard(&saved);
        let current = self.presentation.current.customization.shown(&saved);
        let selected = self.presentation.current.customization.selected();
        let component = selected.and_then(|id| current.component(id));
        let conversations = current
            .views()
            .filter(|(_, c)| *c == Component::Conversation)
            .count();
        let act = |id: &'static str, text: &'static str, action: Action, disabled: bool| {
            Button::new(id, text)
                .size(ButtonSize::Compact)
                .style(if id == "customize-apply" {
                    ButtonStyle::Primary
                } else {
                    ButtonStyle::Subtle
                })
                .disabled(disabled)
                .debug_selector(|| id.into())
                .on_click(self.scoped_listener(cx, move |this, _, window, cx| {
                    this.customize(action.clone(), window, cx)
                }))
        };
        let status = match self.presentation.current.customization.interpretation() {
            Some(Interpretation::Running { .. }) => Some("Asking Claude Code…".into()),
            Some(Interpretation::Failed(error)) => Some(error.clone()),
            Some(Interpretation::Unsupported(reason)) => Some(format!("Not supported. {reason}")),
            _ => self
                .presentation
                .current
                .customization
                .note()
                .map(str::to_owned),
        };
        v_flex()
            .id("customize-panel")
            .capture_any_mouse_down(
                self.scoped_listener(cx, |this, _, window, cx| this.focus.focus(window, cx)),
            )
            .relative()
            .child(self.measure_customize_toolbar(cx))
            .automation_id("customize-panel")
            .flex_none()
            .p_2()
            .gap_1()
            .hairline_b(cx)
            .child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .child(div().flex_1())
                    .child(act(
                        "customize-borrow",
                        "Borrow layout",
                        Action::Borrow,
                        false,
                    ))
                    .child(act("customize-reset", "Reset", Action::Reset, false))
                    .child(act(
                        "customize-undo",
                        "Undo",
                        Action::Undo,
                        !self.presentation.current.customization.can_undo(&saved),
                    ))
                    .child(act("customize-discard", "Discard", Action::Discard, !dirty))
                    .child(act(
                        "customize-apply",
                        "Apply",
                        Action::Apply,
                        !self
                            .presentation
                            .current
                            .customization
                            .can_apply(self.saved_layout(cx), &saved),
                    ))
                    .child(act("customize-close", "Close", Action::Close, false)),
            )
            .child(
                v_flex()
                    .id("customize-body")
                    .min_h_0()
                    .overflow_y_scroll()
                    .gap_1()
                    .max_h(px((f32::from(viewport.height) - 360.).max(28.)))
                    .child(
                        h_flex()
                            .gap_1()
                            .flex_wrap()
                            .child(
                                div()
                                    .id("customize-prompt")
                                    .automation_id("customize-prompt")
                                    .key_context("CustomizePrompt")
                                    .flex_1()
                                    .min_w(px(180.))
                                    .px_2()
                                    .rounded(radius::CONTROL)
                                    .border_1()
                                    .border_color(colors.border)
                                    .on_click(self.scoped_listener(cx, |this, _, window, cx| {
                                        this.presentation
                                            .current
                                            .customize_prompt
                                            .focus_handle(cx)
                                            .focus(window, cx)
                                    }))
                                    .on_action(self.scoped_listener(
                                        cx,
                                        |this, _: &SendMessage, window, cx| {
                                            let prompt = this
                                                .presentation
                                                .current
                                                .customize_prompt
                                                .read(cx)
                                                .value()
                                                .to_owned();
                                            this.customize(Action::Ask(prompt), window, cx);
                                            cx.stop_propagation();
                                        },
                                    ))
                                    .child(self.presentation.current.customize_prompt.clone()),
                            )
                            .child(act(
                                "customize-ask",
                                "Preview",
                                Action::Ask(
                                    self.presentation
                                        .current
                                        .customize_prompt
                                        .read(cx)
                                        .value()
                                        .to_owned(),
                                ),
                                running,
                            ))
                            .when(running, |d| {
                                d.child(act("customize-cancel", "Cancel", Action::Cancel, false))
                            })
                            .when(self.presentation.current.customization.can_retry(), |d| {
                                d.child(act("customize-retry", "Retry", Action::Retry, false))
                            }),
                    )
                    .child(
                        h_flex()
                            .id("component-controls")
                            .gap_1()
                            .h(px(28.))
                            .flex_none()
                            .overflow_x_scroll()
                            .children(current.views().map(|(id, c)| {
                                Button::new(
                                    SharedString::from(format!("select-component-{}", id.0)),
                                    format!("{} {}", label(c), id.0),
                                )
                                .size(ButtonSize::Compact)
                                .selected(selected == Some(id))
                                .debug_selector(move || format!("select-component-{}", id.0))
                                .on_click(self.scoped_listener(cx, move |this, _, window, cx| {
                                    this.customize(Action::Select(id), window, cx)
                                }))
                            }))
                            .children(selected.map(|id| {
                                act(
                                    "component-duplicate",
                                    "Duplicate conversation",
                                    Action::Duplicate(id),
                                    component != Some(Component::Conversation),
                                )
                            }))
                            .children(selected.map(|id| {
                                act(
                                    "component-remove",
                                    "Remove",
                                    Action::Edit(Edit::Close(id)),
                                    component == Some(Component::Composer)
                                        || (component == Some(Component::Conversation)
                                            && conversations == 1),
                                )
                            })),
                    )
                    .child(
                        h_flex()
                            .id("component-placement")
                            .gap_1()
                            .children(
                                [
                                    ("component-left", "←", "Move left", Axis::Row, false),
                                    ("component-right", "→", "Move right", Axis::Row, true),
                                    ("component-up", "↑", "Move up", Axis::Column, false),
                                    ("component-down", "↓", "Move down", Axis::Column, true),
                                ]
                                .map(
                                    |(id, text, name, axis, after)| {
                                        Button::new(id, text)
                                            .size(ButtonSize::Compact)
                                            .disabled(selected.is_none())
                                            .debug_selector(|| id.into())
                                            .tooltip(Tooltip::with_key(name, format!("Alt+{text}")))
                                            .on_click(self.scoped_listener(
                                                cx,
                                                move |this, _, window, cx| {
                                                    this.move_component(axis, after, window, cx)
                                                },
                                            ))
                                    },
                                ),
                            )
                            .children(
                                [
                                    ("component-size-auto", "Auto", Size::Auto),
                                    (
                                        "component-size-px",
                                        "Pixels",
                                        Size::Px(NonZeroU16::new(320).unwrap()),
                                    ),
                                    ("component-size-fill", "Fill", Size::Fill(NonZeroU16::MIN)),
                                ]
                                .map(|(id, text, mode)| {
                                    Button::new(id, text)
                                        .size(ButtonSize::Compact)
                                        .disabled(selected.is_none())
                                        .debug_selector(|| id.into())
                                        .when(!matches!(mode, Size::Auto), |button| {
                                            button.tooltip(Tooltip::text("Alt+[ / Alt+]"))
                                        })
                                        .on_click(self.scoped_listener(
                                            cx,
                                            move |this, _, window, cx| {
                                                this.size_component(0, Some(mode), window, cx)
                                            },
                                        ))
                                }),
                            )
                            .when(
                                !current.views().any(|(_, c)| c == Component::SessionList),
                                |d| {
                                    d.child(act(
                                        "component-add-sidebar",
                                        "Add session list",
                                        Action::AddSessionList,
                                        false,
                                    ))
                                },
                            )
                            .when(!current.views().any(|(_, c)| c == Component::Files), |d| {
                                d.child(act(
                                    "component-add-files",
                                    "Add files",
                                    Action::AddFiles,
                                    false,
                                ))
                            })
                            .when(
                                !current.views().any(|(_, c)| c == Component::Changes),
                                |d| {
                                    d.child(act(
                                        "component-add-changes",
                                        "Add changes",
                                        Action::AddChanges,
                                        false,
                                    ))
                                },
                            ),
                    ),
            )
            .children(status.map(|status| {
                div()
                    .automation_id("customize-status")
                    .child(Label::new(status).size(TextSize::Sm).color(Color::Muted))
            }))
            .children(self.presentation.project.as_ref().err().map(|error| {
                Label::new(format!("Layout cannot be saved. {error}"))
                    .size(TextSize::Sm)
                    .color(Color::Warning)
            }))
    }

    pub(crate) fn borrow_dialog(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let chooser = self.presentation.current.customization.chooser()?;
        let focus = self.presentation.current.borrow_focus.as_ref()?;
        let colors = cx.theme().colors;
        let close = |id: &'static str, text: &'static str, action: Action| {
            Button::new(id, text)
                .size(ButtonSize::Compact)
                .style(if id == "borrow-preview" || id == "borrow-replace" {
                    ButtonStyle::Primary
                } else {
                    ButtonStyle::Subtle
                })
                .debug_selector(|| id.into())
                .on_click(self.scoped_listener(cx, move |this, _, window, cx| {
                    this.customize(action.clone(), window, cx)
                }))
        };
        let sources: Vec<_> = self
            .state
            .read(cx)
            .store()
            .workspace()
            .preferences()
            .layouts_in(self.window_id)
            .filter(|(id, _)| self.presentation.project.as_ref().ok() != Some(id))
            .collect();
        let title = match chooser {
            Chooser::Replace(_) => "Replace this preview?",
            Chooser::Unavailable => "This layout cannot be used",
            Chooser::Layouts { .. } if sources.is_empty() => "No other saved layouts",
            _ => "Borrow a layout",
        };
        let mut card = v_flex()
            .id("borrow-card")
            .key_context("Dialog")
            .track_focus(focus)
            .w(px(560.))
            .max_w_full()
            .max_h_full()
            .p_5()
            .gap_3()
            .surface(Elevation::Overlay, cx)
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                h_flex()
                    .child(
                        Label::new(title)
                            .size(TextSize::Body)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .child(div().flex_1())
                    .child(close("borrow-close", "×", Action::CloseChooser)),
            );
        match chooser {
            Chooser::Replace(_) => {
                card = card.child(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(close(
                            "borrow-keep",
                            "Keep current preview",
                            Action::CloseChooser,
                        ))
                        .child(close(
                            "borrow-replace",
                            "Replace preview",
                            Action::ReplaceBorrow,
                        )),
                );
            }
            Chooser::Unavailable => {
                card = card.child(close(
                    "borrow-back",
                    "Back to layouts",
                    Action::BackToLayouts,
                ));
            }
            Chooser::Layouts { selected } if !sources.is_empty() => {
                card = card.child(
                    v_flex()
                        .id("borrow-sources")
                        .min_h_0()
                        .overflow_y_scroll()
                        .gap_2()
                        .children(sources.into_iter().enumerate().map(|(i, (id, raw))| {
                            let chosen = selected.as_ref() == Some(&id);
                            let path = id.path().display().to_string();
                            let count = raw.supported().map(|c| {
                                c.views()
                                    .filter(|(_, k)| *k == Component::Conversation)
                                    .count()
                            });
                            v_flex()
                                .id(SharedString::from(format!("borrow-source-{i}")))
                                .automation_id(format!("borrow-source-{i}"))
                                .p_3()
                                .gap_1()
                                .rounded(radius::CONTROL)
                                .border_1()
                                .bg(if chosen {
                                    colors.selection_fill
                                } else {
                                    colors.background
                                })
                                .border_color(if chosen {
                                    colors.selection_ring
                                } else {
                                    colors.border
                                })
                                .cursor_pointer()
                                .child(Label::new(path).color(if chosen {
                                    Color::Strong
                                } else {
                                    Color::Muted
                                }))
                                .child(
                                    Label::new(count.map_or_else(
                                        || "Unsupported saved layout".into(),
                                        |n| format!("{n} conversation views"),
                                    ))
                                    .size(TextSize::Sm)
                                    .color(Color::Muted),
                                )
                                .on_click(self.scoped_listener(cx, move |this, _, window, cx| {
                                    this.customize(Action::Choose(id.clone()), window, cx)
                                }))
                        })),
                );
                card = card.child(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(close("borrow-cancel", "Cancel", Action::CloseChooser))
                        .child(
                            close("borrow-preview", "Preview layout", Action::PreviewBorrow)
                                .disabled(selected.is_none()),
                        ),
                );
            }
            _ => {
                card = card.child(close("borrow-empty-close", "Close", Action::CloseChooser));
            }
        }
        Some(
            div()
                .id("borrow-dialog")
                .automation_id("borrow-dialog")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .p_4()
                .occlude()
                .bg(colors.background.opacity(0.7))
                .child(card)
                .into_any_element(),
        )
    }
}

#[cfg(all(test, feature = "ui-tests"))]
mod tests {
    use super::*;
    use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext};
    use keystone_core::{ProjectId, Store};
    use std::path::Path;

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
        cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        })
    }
    fn click(cx: &mut VisualTestContext, name: &'static str) {
        let bounds = cx
            .debug_bounds(name)
            .unwrap_or_else(|| panic!("missing {name}"));
        cx.simulate_click(bounds.center(), Modifiers::none());
        cx.run_until_parked();
    }
    #[gpui::test]
    fn keyboard_customize_duplicates_applies_and_undo_restores_absence(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let (view, cx) = open(&path, cx);
        click(cx, "welcome-codex");
        cx.simulate_input("keep this draft");
        cx.simulate_keystrokes("secondary-shift-l");
        cx.run_until_parked();
        assert!(cx.debug_bounds("customize-panel").is_some());
        click(cx, "component-duplicate");
        view.read_with(cx, |this, cx| {
            assert_eq!(
                this.presentation
                    .current
                    .customization
                    .shown(&this.saved_composition(cx))
                    .views()
                    .filter(|(_, c)| *c == Component::Conversation)
                    .count(),
                2
            );
            assert_eq!(this.composer.read(cx).value(), "keep this draft");
            assert!(this.saved_layout(cx).is_none());
        });
        cx.simulate_keystrokes("secondary-s");
        cx.run_until_parked();
        view.read_with(cx, |this, cx| {
            assert_eq!(
                this.saved_layout(cx)
                    .unwrap()
                    .supported()
                    .unwrap()
                    .views()
                    .filter(|(_, c)| *c == Component::Conversation)
                    .count(),
                2
            );
        });
        click(cx, "customize-undo");
        view.read_with(cx, |this, cx| assert!(this.saved_layout(cx).is_none()));
        click(cx, "customize-borrow");
        assert!(cx.debug_bounds("borrow-empty-close").is_some());
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(cx.debug_bounds("borrow-dialog").is_none());
        assert!(cx.debug_bounds("customize-panel").is_some());
    }
    #[gpui::test]
    fn undo_is_withdrawn_once_a_live_edit_changes_the_applied_layout(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let (view, cx) = open(&path, cx);
        click(cx, "welcome-codex");
        click(cx, "customize-toggle");
        click(cx, "component-duplicate");
        click(cx, "customize-apply");
        click(cx, "customize-close");
        cx.simulate_keystrokes("secondary-shift-e");
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("files-tab-0").is_some(),
            "Files opened live"
        );
        click(cx, "customize-toggle");
        click(cx, "customize-undo");
        assert!(
            cx.debug_bounds("files-tab-0").is_some(),
            "Undo cannot take back a layout that changed since Apply"
        );
        view.read_with(cx, |this, cx| {
            assert!(
                this.saved_composition(cx)
                    .views()
                    .any(|(_, c)| c == Component::Files)
            );
        });
    }
    #[gpui::test]
    fn borrow_confirmation_preserves_preview_then_stages_an_independent_copy(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let source_dir = dir.path().join("source");
        std::fs::create_dir(&source_dir).unwrap();
        let source = ProjectId::new(&source_dir).unwrap();
        let raw = SavedComposition::from(Composition::default());
        Store::open(&path)
            .unwrap()
            .set_project_layout(&source, Some(raw.clone()))
            .unwrap();
        let (view, cx) = open(&path, cx);
        click(cx, "customize-toggle");
        click(cx, "component-duplicate");
        let candidate = view.read_with(cx, |this, cx| {
            this.presentation
                .current
                .customization
                .shown(&this.saved_composition(cx))
                .clone()
        });
        click(cx, "customize-borrow");
        click(cx, "borrow-source-0");
        click(cx, "borrow-preview");
        assert!(cx.debug_bounds("borrow-keep").is_some());
        click(cx, "borrow-keep");
        view.read_with(cx, |this, cx| {
            assert_eq!(
                this.presentation
                    .current
                    .customization
                    .shown(&this.saved_composition(cx)),
                &candidate
            )
        });
        click(cx, "customize-borrow");
        click(cx, "borrow-source-0");
        click(cx, "borrow-preview");
        click(cx, "borrow-replace");
        view.read_with(cx, |this, cx| {
            let saved = this.saved_composition(cx);
            let next = this.presentation.current.customization.shown(&saved);
            assert!(
                next.views()
                    .all(|(id, _)| !candidate.views().any(|(old, _)| id == old))
            );
            assert_eq!(
                this.state
                    .read(cx)
                    .store()
                    .workspace()
                    .preferences()
                    .project_layout(&source),
                Some(&raw)
            );
        });
        click(cx, "customize-discard");
        view.read_with(cx, |this, cx| {
            assert_eq!(
                this.presentation
                    .current
                    .customization
                    .shown(&this.saved_composition(cx)),
                &this.saved_composition(cx)
            )
        });
    }
    #[gpui::test]
    fn preview_reads_the_source_after_the_chooser_was_opened(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let source_dir = dir.path().join("source");
        std::fs::create_dir(&source_dir).unwrap();
        let source = ProjectId::new(&source_dir).unwrap();
        let original = Composition::default();
        Store::open(&path)
            .unwrap()
            .set_project_layout(&source, Some(original.clone().into()))
            .unwrap();
        let (view, cx) = open(&path, cx);
        click(cx, "customize-toggle");
        click(cx, "customize-borrow");
        click(cx, "borrow-source-0");
        let revised = original
            .edit(Edit::Open {
                view: keystone_core::ViewId(99),
                component: Component::Conversation,
                at: Place::TabWith(keystone_core::ViewId::MAIN),
                size: Size::Fill(NonZeroU16::MIN),
            })
            .unwrap();
        view.update(cx, |this, cx| {
            this.state
                .update(cx, |state, cx| {
                    state.set_project_layout(&source, Some(revised.into()), cx)
                })
                .unwrap();
        });
        click(cx, "borrow-preview");
        view.read_with(cx, |this, cx| {
            let saved = this.saved_composition(cx);
            let candidate = this.presentation.current.customization.shown(&saved);
            assert_eq!(
                candidate
                    .views()
                    .filter(|(_, c)| *c == Component::Conversation)
                    .count(),
                2
            );
            assert!(this.saved_layout(cx).is_none());
        });
    }

    #[gpui::test]
    fn a_short_window_keeps_decisions_and_send_clear_of_each_other(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (_view, cx) = open(&dir.path().join("workspace.json"), cx);
        click(cx, "welcome-codex");
        cx.simulate_resize(gpui::size(px(640.), px(360.)));
        cx.run_until_parked();
        click(cx, "customize-toggle");
        let panel = cx.debug_bounds("customize-panel").unwrap();
        let send = cx.debug_bounds("send").unwrap();
        assert!(panel.bottom() <= send.top());
        assert!(send.bottom() <= px(360.));
        for id in [
            "customize-apply",
            "customize-discard",
            "customize-undo",
            "customize-borrow",
        ] {
            let button = cx.debug_bounds(id).unwrap();
            assert!(button.bottom() < send.top());
        }
    }

    #[gpui::test]
    fn moving_and_resizing_keep_the_same_composer_and_candidate_only(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let (view, cx) = open(&path, cx);
        click(cx, "welcome-codex");
        cx.simulate_input("same editor");
        let composer = view.read_with(cx, |this, _cx| this.composer.clone());
        click(cx, "customize-toggle");
        click(cx, "select-component-2");
        click(cx, "component-right");
        click(cx, "component-size-px");
        cx.simulate_keystrokes("alt-]");
        cx.run_until_parked();
        view.read_with(cx, |this, cx| {
            assert_eq!(this.composer, composer);
            assert_eq!(this.composer.read(cx).value(), "same editor");
            assert!(this.saved_layout(cx).is_none());
            assert_ne!(
                this.presentation
                    .current
                    .customization
                    .shown(&this.saved_composition(cx)),
                &this.saved_composition(cx)
            );
        });
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        view.read_with(cx, |this, cx| {
            assert!(!this.presentation.current.customization.is_open());
            assert!(this.saved_layout(cx).is_none());
        });
        assert!(cx.debug_bounds("composer-field").is_some());
    }
}
