//! The card for a question a provider waits on: Claude's `AskUserQuestion`, Codex's
//! `request_user_input`, or an MCP server's form. It shows under the transcript, after
//! any approval, and takes answers by mouse or keyboard. The answers live with the
//! app's conversation ([`crate::conversation::Asked`]), so a closed window loses none;
//! a window keeps only the focus handles and text editors, rebuilt from them.

use crate::app_state::AppState;
use crate::arrangement;
use crate::conversation::{Asked, Conversation};
use crate::view_state::ViewKey;
use gpui::{
    AnyElement, App, Entity, FocusHandle, Focusable, MouseButton, SharedString, Subscription,
    Window, div, prelude::*, px,
};
use keystone_agent::{Answer, Input, RequestId};
use keystone_ui::prelude::*;
use keystone_ui::text_input::Changed;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// Answers the question: `false` sends the answers, `true` declines it.
pub(crate) type Done = Rc<dyn Fn(bool, &mut Window, &mut App)>;

/// A window's focus handles and text editors for the questions it shows, by view.
#[derive(Default)]
pub(crate) struct Controls {
    focus: HashMap<(ViewKey, String), FocusHandle>,
    fields: HashMap<(ViewKey, RequestId, usize), (Entity<TextInput>, Subscription)>,
}

impl Controls {
    /// Forgets the editors of questions no longer waiting in any of `conversations`:
    /// answered here or in another window, withdrawn, or ended with their turn.
    pub(crate) fn retain_waiting<'a>(
        &mut self,
        conversations: impl Iterator<Item = &'a Conversation>,
    ) {
        if self.fields.is_empty() {
            return;
        }
        let waiting: Vec<&RequestId> = conversations
            .flat_map(|c| c.questions.iter().map(|q| &q.question.request))
            .collect();
        self.fields
            .retain(|(_, request, _), _| waiting.contains(&request));
    }

    fn focus(&mut self, key: ViewKey, name: &str, cx: &mut App) -> FocusHandle {
        self.focus
            .entry((key, name.to_owned()))
            .or_insert_with(|| cx.focus_handle().tab_stop(true))
            .clone()
    }
}

#[derive(IntoElement)]
pub(crate) struct QuestionCard {
    pub asked: Asked,
    /// Questions waiting after this one.
    pub more: usize,
    /// Who asks when the agent itself does, such as "Claude Code".
    pub agent: &'static str,
    pub key: ViewKey,
    /// This view is the one the keyboard answers in, so its controls take focus.
    pub keyboard: bool,
    pub controls: Rc<RefCell<Controls>>,
    pub state: Entity<AppState>,
    pub done: Done,
}

impl QuestionCard {
    fn selector(&self, name: &str) -> SharedString {
        arrangement::selector(name, self.key.view)
    }

    fn edit<E: Fn(&mut Answer) + 'static>(
        &self,
        field: usize,
        edit: E,
    ) -> impl Fn(&mut App) + use<E> {
        let (state, session) = (self.state.clone(), self.key.session);
        let request = self.asked.question.request.clone();
        let edit = Rc::new(edit);
        move |cx| {
            let edit = edit.clone();
            state.update(cx, |state, cx| {
                state.edit_answer(session, &request, field, |a| edit(a), cx)
            });
        }
    }

    fn option(
        &self,
        field: usize,
        index: usize,
        label: &str,
        description: Option<&str>,
        multiple: bool,
        cx: &mut App,
    ) -> AnyElement {
        let on = self.asked.answers[field].chosen.contains(&index);
        let name = format!("question-{field}-{index}");
        let focus = self
            .keyboard
            .then(|| self.controls.borrow_mut().focus(self.key, &name, cx));
        let edit = self.edit(field, move |answer| {
            if multiple {
                if let Some(at) = answer.chosen.iter().position(|&c| c == index) {
                    answer.chosen.remove(at);
                } else {
                    answer.chosen.push(index);
                }
            } else if answer.chosen == [index] {
                answer.chosen.clear();
            } else {
                answer.chosen = vec![index];
                answer.text.clear();
            }
        });
        let selector = self.selector(&name);
        Button::new(selector.clone(), label.to_owned())
            .style(ButtonStyle::Subtle)
            .selected(on)
            .when(on, |b| b.icon(IconName::Check))
            .when_some(description, |b, d| b.tooltip(Tooltip::text(d.to_owned())))
            .when_some(focus, |b, focus| b.track_focus(&focus).reveal_on_focus())
            .debug_selector(move || selector.to_string())
            .on_click(move |_, _, cx| edit(cx))
            .into_any_element()
    }

    /// The text editor for field `field`, made the first time it shows in this view.
    fn text(
        &self,
        field: usize,
        placeholder: &str,
        clears_choice: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let request = self.asked.question.request.clone();
        let current = self.asked.answers[field].text.clone();
        let input = {
            let mut controls = self.controls.borrow_mut();
            let slot = (self.key, request.clone(), field);
            if let Some((input, _)) = controls.fields.get(&slot) {
                input.clone()
            } else {
                let input = cx.new(|cx| {
                    let mut input = TextInput::new(placeholder, InputMode::SingleLine, cx);
                    input.set_value(current.clone(), cx);
                    input
                });
                let (state, session) = (self.state.clone(), self.key.session);
                let changed = cx.subscribe(&input, move |input, _: &Changed, cx| {
                    let text = input.read(cx).value().to_owned();
                    state.update(cx, |state, cx| {
                        state.edit_answer(
                            session,
                            &request,
                            field,
                            |answer| {
                                if clears_choice && !text.trim().is_empty() {
                                    answer.chosen.clear();
                                }
                                answer.text = text;
                            },
                            cx,
                        )
                    });
                });
                controls.fields.insert(slot, (input.clone(), changed));
                input
            }
        };
        let focus = input.read(cx).focus_handle(cx);
        let _ = focus.clone().tab_stop(self.keyboard);
        let focused = focus.is_focused(window);
        // Another view may have changed the answer; the field being typed in keeps its
        // own text and selection.
        if !focused && input.read(cx).value() != current {
            input.update(cx, |input, cx| input.set_value(current, cx));
        }
        let colors = cx.theme().colors;
        let selector = self.selector(&format!("question-{field}-text"));
        div()
            .id(selector.clone())
            .automation_id(selector.clone())
            .debug_selector(move || selector.to_string())
            .w_full()
            .max_w(px(360.))
            .min_h(px(28.))
            .px_2()
            .py_1()
            .rounded(radius::CONTROL)
            .border_1()
            .border_color(if focused {
                colors.border_focused
            } else {
                colors.border
            })
            .bg(colors.background)
            // The conversation takes focus on a click inside it; the field keeps it.
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                focus.focus(window, cx);
                cx.stop_propagation();
            })
            .child(input)
            .into_any_element()
    }
}

impl RenderOnce for QuestionCard {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let colors = cx.theme().colors;
        let question = &self.asked.question;
        let asker = match &question.server {
            Some(server) => format!("The {server} MCP server asks"),
            None => format!("{} asks", self.agent),
        };
        let heading = match self.more {
            0 => asker,
            n => format!("{asker} · {n} more waiting"),
        };
        let problem = question.problem(&self.asked.answers);
        let mut card = v_flex()
            .id(self.selector("question"))
            .automation_id(self.selector("question"))
            .key_context("Question")
            .surface(Elevation::Raised, cx)
            // A raised block draws no hairline; a question needs the user, as an
            // approval does, so it adds its own.
            .border_1()
            .border_color(colors.border_strong)
            .p_3()
            .gap_3()
            .child(Label::new(heading).size(TextSize::Ui).color(Color::Muted))
            .when_some(question.message.clone(), |d, message| {
                d.child(Label::new(message).size(TextSize::Ui))
            });
        for (index, field) in question.fields.iter().enumerate() {
            let mut row = v_flex().gap_1();
            match &field.prompt {
                Some(prompt) => {
                    row = row
                        .child(
                            Label::new(field.header.clone())
                                .size(TextSize::Sm)
                                .color(Color::Muted),
                        )
                        .child(Label::new(prompt.clone()).size(TextSize::Ui));
                }
                None => row = row.child(Label::new(field.header.clone()).size(TextSize::Ui)),
            }
            match &field.input {
                Input::Choice {
                    options,
                    multiple,
                    other,
                } => {
                    if *multiple {
                        row = row.child(
                            Label::new("Choose any that apply")
                                .size(TextSize::Sm)
                                .color(Color::Muted),
                        );
                    }
                    let buttons: Vec<AnyElement> = options
                        .iter()
                        .enumerate()
                        .map(|(n, option)| {
                            self.option(
                                index,
                                n,
                                &option.label,
                                option.description.as_deref(),
                                *multiple,
                                cx,
                            )
                        })
                        .collect();
                    row = row.child(h_flex().flex_wrap().gap_2().children(buttons));
                    if *other {
                        row = row.child(self.text(index, "Other answer", !multiple, window, cx));
                    }
                }
                Input::Text => row = row.child(self.text(index, "Your answer", false, window, cx)),
                Input::Number { .. } => {
                    row = row.child(self.text(index, "A number", false, window, cx))
                }
            }
            card = card.child(row);
        }
        let submit = self.selector("question-submit");
        let decline = self.selector("question-decline");
        let (submit_focus, decline_focus) = if self.keyboard {
            let mut controls = self.controls.borrow_mut();
            (
                Some(controls.focus(self.key, "question-submit", cx)),
                Some(controls.focus(self.key, "question-decline", cx)),
            )
        } else {
            (None, None)
        };
        let (send, refuse) = (self.done.clone(), self.done.clone());
        card.child(
            h_flex()
                .gap_2()
                .child(
                    Button::new(submit.clone(), "Submit")
                        .style(ButtonStyle::Primary)
                        .icon(IconName::Check)
                        .disabled(problem.is_some())
                        .when_some(submit_focus, |b, focus| b.track_focus(&focus))
                        .debug_selector(move || submit.to_string())
                        .on_click(move |_, window, cx| send(false, window, cx)),
                )
                .child(
                    Button::new(decline.clone(), "Decline")
                        .style(ButtonStyle::Subtle)
                        .icon(IconName::Close)
                        .when_some(decline_focus, |b, focus| b.track_focus(&focus))
                        .debug_selector(move || decline.to_string())
                        .on_click(move |_, window, cx| refuse(true, window, cx)),
                )
                .when_some(problem, |d, problem| {
                    d.child(Label::new(problem).size(TextSize::Sm).color(Color::Muted))
                }),
        )
    }
}

#[cfg(all(test, feature = "ui-tests"))]
#[path = "questions_tests.rs"]
mod tests;
