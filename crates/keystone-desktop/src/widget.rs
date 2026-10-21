//! How an agent-built widget looks: Keystone's A2UI catalog drawn with its own
//! components. Inputs write to the widget's data model and Buttons press through the
//! callbacks the transcript passes in; nothing here keeps state of its own except the
//! text fields' editors. See `docs/design/widgets.md`.

use gpui::{
    AnyElement, App, Context, Entity, Focusable, FontWeight, MouseButton, SharedString,
    Subscription, Window, div, prelude::*, px,
};
use keystone_agent::surface::{
    Align, ButtonVariant, Component, FieldVariant, Justify, Kind, Status, Surface, TextVariant,
};
use keystone_ui::prelude::*;
use keystone_ui::text_input::Changed;
use keystone_ui::{Slider, spacing};
use serde_json::Value;
use std::rc::Rc;

/// Writes `value` at a JSON Pointer in the widget's data model.
pub type EditHandler = Rc<dyn Fn(&str, Value, &mut App)>;
/// Presses the Button with this component ID.
pub type PressHandler = Rc<dyn Fn(&str, &mut App)>;

/// What a widget can do to the app: write a bound value, or press a Button.
#[derive(Clone)]
pub struct WidgetControl {
    pub on_edit: EditHandler,
    pub on_press: PressHandler,
    /// The automation ID of the widget; each component's is `{selector}-{id}`.
    pub selector: String,
}

#[derive(IntoElement)]
pub struct SurfaceView {
    surface: Surface,
    control: WidgetControl,
}

impl SurfaceView {
    pub fn new(surface: Surface, control: WidgetControl) -> Self {
        Self { surface, control }
    }
}

/// A text field's editor, kept while its row is drawn.
struct Field {
    input: Entity<TextInput>,
    _changed: Subscription,
}

struct Draw<'a> {
    surface: &'a Surface,
    control: &'a WidgetControl,
}

impl RenderOnce for SurfaceView {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let draw = Draw {
            surface: &self.surface,
            control: &self.control,
        };
        let root = draw.component("root", None, 0, window, cx);
        let (note, color) = match self.surface.status {
            Status::Waiting => ("Waiting for your answer".to_owned(), Color::Warning),
            Status::Answered => (
                format!(
                    "Sent {}",
                    self.surface.last_action.as_deref().unwrap_or("an answer")
                ),
                Color::Muted,
            ),
            Status::Stopped => (
                "The turn that asked has ended, so a button sends a new message.".to_owned(),
                Color::Muted,
            ),
            Status::Shown => (String::new(), Color::Muted),
        };
        v_flex()
            .automation_id(self.control.selector.clone())
            .max_w(px(620.))
            .gap_1()
            .child(root)
            .when(!note.is_empty(), |d| {
                d.child(
                    div()
                        .automation_id(format!("{}-status", self.control.selector))
                        .child(Label::new(note).size(TextSize::Sm).color(color)),
                )
            })
    }
}

impl Draw<'_> {
    fn id(&self, component: &str) -> String {
        format!("{}-{component}", self.control.selector)
    }

    fn element_id(&self, component: &str) -> SharedString {
        format!("widget-{}-{component}", self.surface.call).into()
    }

    /// One component and, inside a Row or Column, the parent's direction for `weight`.
    fn component(
        &self,
        id: &str,
        parent: Option<bool>,
        depth: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        // Validation already bounds the depth and refuses missing children.
        let Some(component) = self.surface.component(id).filter(|_| depth < 32) else {
            return div().into_any_element();
        };
        let element = self.kind(component, depth, window, cx);
        let element = div().automation_id(self.id(id)).child(element);
        match (component.weight, parent) {
            (Some(weight), Some(_)) => {
                let mut element = element;
                element.style().flex_grow = Some(weight);
                element.style().flex_shrink = Some(1.);
                element.style().flex_basis = Some(px(0.).into());
                element.min_w_0().into_any_element()
            }
            _ => element.into_any_element(),
        }
    }

    fn kind(
        &self,
        component: &Component,
        depth: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let colors = cx.theme().colors;
        let id = component.id.as_str();
        match &component.kind {
            Kind::Column {
                children,
                justify,
                align,
            }
            | Kind::Row {
                children,
                justify,
                align,
            } => {
                let row = matches!(component.kind, Kind::Row { .. });
                let base = if row {
                    h_flex().gap(spacing::SM).flex_wrap()
                } else {
                    v_flex().gap(spacing::SM)
                };
                let base = match justify {
                    Justify::Start | Justify::Stretch => base.justify_start(),
                    Justify::Center => base.justify_center(),
                    Justify::End => base.justify_end(),
                    Justify::SpaceBetween => base.justify_between(),
                    Justify::SpaceAround => base.justify_around(),
                    Justify::SpaceEvenly => base.justify_evenly(),
                };
                let base = match (align, row) {
                    (Some(Align::Start), _) => base.items_start(),
                    (Some(Align::Center), _) | (None, true) => base.items_center(),
                    (Some(Align::End), _) => base.items_end(),
                    (Some(Align::Stretch), _) | (None, false) => base,
                };
                base.children(
                    children
                        .iter()
                        .map(|child| self.component(child, Some(row), depth + 1, window, cx))
                        .collect::<Vec<_>>(),
                )
                .into_any_element()
            }
            Kind::Card { child } => v_flex()
                .surface(Elevation::Raised, cx)
                .p_3()
                .child(self.component(child, None, depth + 1, window, cx))
                .into_any_element(),
            Kind::Text { text, variant } => {
                let (size, weight, color) = match variant {
                    TextVariant::H1 => (TextSize::Title, FontWeight::SEMIBOLD, Color::Default),
                    TextVariant::H2 => (TextSize::Title, FontWeight::MEDIUM, Color::Default),
                    TextVariant::H3 => (TextSize::Body, FontWeight::SEMIBOLD, Color::Default),
                    TextVariant::H4 => (TextSize::Base, FontWeight::SEMIBOLD, Color::Default),
                    TextVariant::H5 => (TextSize::Ui, FontWeight::SEMIBOLD, Color::Default),
                    TextVariant::Caption => (TextSize::Sm, FontWeight::NORMAL, Color::Muted),
                    TextVariant::Body => (TextSize::Base, FontWeight::NORMAL, Color::Default),
                };
                Label::new(self.surface.text(text))
                    .size(size)
                    .weight(weight)
                    .color(color)
                    .into_any_element()
            }
            Kind::Divider { vertical } => {
                let line = div().flex_none().bg(colors.border);
                if *vertical {
                    line.w(px(1.)).self_stretch().into_any_element()
                } else {
                    line.h(px(1.)).w_full().into_any_element()
                }
            }
            Kind::Button { child, variant, .. } => {
                let label = match self.surface.component(child).map(|c| &c.kind) {
                    Some(Kind::Text { text, .. }) => self.surface.text(text),
                    _ => String::new(),
                };
                let press = self.control.on_press.clone();
                let component = id.to_owned();
                Button::new(gpui::ElementId::Name(self.element_id(id)), label)
                    .style(match variant {
                        ButtonVariant::Primary => ButtonStyle::Primary,
                        ButtonVariant::Default => ButtonStyle::Subtle,
                        ButtonVariant::Borderless => ButtonStyle::Ghost,
                    })
                    .debug_selector({
                        let id = self.id(&format!("{id}-button"));
                        move || id
                    })
                    .on_click(move |_, _, cx| press(&component, cx))
                    .into_any_element()
            }
            Kind::CheckBox { label, value } => {
                let on = self.surface.value(value) == Some(&Value::Bool(true));
                self.check(id, self.surface.text(label), on, value, Value::Bool(!on))
            }
            Kind::TextField {
                label,
                value,
                variant,
            } => {
                let text = match self.surface.value(value) {
                    Some(Value::String(text)) => text.clone(),
                    Some(Value::Null) | None => String::new(),
                    Some(other) => other.to_string(),
                };
                let mode = if *variant == FieldVariant::LongText {
                    InputMode::Multiline
                } else {
                    InputMode::SingleLine
                };
                let on_edit = self.control.on_edit.clone();
                let pointer = value.clone();
                let current = text.clone();
                let field = window.use_keyed_state(
                    gpui::ElementId::Name(self.element_id(id)),
                    cx,
                    move |_, cx: &mut Context<Field>| {
                        let input = cx.new(|cx| {
                            let mut input = TextInput::new("", mode, cx);
                            input.set_value(text, cx);
                            input
                        });
                        let _ = input.read(cx).focus_handle(cx).tab_stop(true);
                        let changed = cx.subscribe(&input, move |_, input, _: &Changed, cx| {
                            let text = input.read(cx).value().to_owned();
                            on_edit(&pointer, Value::String(text), cx);
                        });
                        Field {
                            input,
                            _changed: changed,
                        }
                    },
                );
                let input = field.read(cx).input.clone();
                let focus = input.read(cx).focus_handle(cx);
                // Another field or view may have changed the value; the one being typed
                // in keeps its own text and selection.
                if !focus.is_focused(window) && input.read(cx).value() != current {
                    input.update(cx, |input, cx| input.set_value(current, cx));
                }
                v_flex()
                    .gap_1()
                    .child(
                        Label::new(self.surface.text(label))
                            .size(TextSize::Sm)
                            .color(Color::Muted),
                    )
                    .child(
                        div()
                            .automation_id(self.id(&format!("{id}-input")))
                            .debug_selector({
                                let id = self.id(&format!("{id}-field"));
                                move || id
                            })
                            .w_full()
                            .min_h(px(28.))
                            .when(mode == InputMode::Multiline, |d| d.min_h(px(64.)))
                            .px_2()
                            .py_1()
                            .rounded(radius::CONTROL)
                            .border_1()
                            .border_color(colors.border)
                            .bg(colors.background)
                            // The conversation takes focus on a click inside it; the
                            // field keeps it.
                            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                                focus.focus(window, cx);
                                cx.stop_propagation();
                            })
                            .child(input),
                    )
                    .into_any_element()
            }
            Kind::ChoicePicker {
                label,
                options,
                value,
                multiple,
                chips,
            } => {
                let chosen: Vec<String> = self
                    .surface
                    .value(value)
                    .and_then(Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(|v| v.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                let choices = options.iter().map(|option| {
                    let on = chosen.contains(&option.value);
                    let next: Vec<String> = match (*multiple, on) {
                        (true, true) => chosen
                            .iter()
                            .filter(|v| **v != option.value)
                            .cloned()
                            .collect(),
                        (true, false) => chosen
                            .iter()
                            .cloned()
                            .chain([option.value.clone()])
                            .collect(),
                        (false, _) => vec![option.value.clone()],
                    };
                    let next = Value::from(next);
                    let key = format!("{id}-{}", option.value);
                    let text = self.surface.text(&option.label);
                    if *chips {
                        let on_edit = self.control.on_edit.clone();
                        let pointer = value.clone();
                        Button::new(gpui::ElementId::Name(self.element_id(&key)), text)
                            .style(ButtonStyle::Subtle)
                            .selected(on)
                            .when(on, |b| b.icon(IconName::Check))
                            .debug_selector({
                                let id = self.id(&key);
                                move || id
                            })
                            .on_click(move |_, _, cx| on_edit(&pointer, next.clone(), cx))
                            .into_any_element()
                    } else {
                        self.check(&key, text, on, value, next)
                    }
                });
                let choices = if *chips {
                    h_flex()
                        .gap_1()
                        .flex_wrap()
                        .children(choices.collect::<Vec<_>>())
                } else {
                    v_flex().children(choices.collect::<Vec<_>>())
                };
                v_flex()
                    .gap_1()
                    .when_some(label.as_ref(), |d, label| {
                        d.child(
                            Label::new(self.surface.text(label))
                                .size(TextSize::Sm)
                                .color(Color::Muted),
                        )
                    })
                    .child(choices)
                    .into_any_element()
            }
            Kind::Slider {
                label,
                min,
                max,
                value,
            } => {
                let current = self
                    .surface
                    .value(value)
                    .and_then(Value::as_f64)
                    .unwrap_or(*min)
                    .clamp(*min, *max);
                let whole = min.fract() == 0. && max.fract() == 0.;
                let step = if whole {
                    1.
                } else {
                    ((max - min) / 100.) as f32
                };
                let on_edit = self.control.on_edit.clone();
                let pointer = value.clone();
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .justify_between()
                            .child(
                                Label::new(
                                    label
                                        .as_ref()
                                        .map(|l| self.surface.text(l))
                                        .unwrap_or_default(),
                                )
                                .size(TextSize::Sm)
                                .color(Color::Muted),
                            )
                            .child(
                                Label::new(if whole {
                                    format!("{current:.0}")
                                } else {
                                    format!("{current:.2}")
                                })
                                .size(TextSize::Sm)
                                .color(Color::Muted),
                            ),
                    )
                    .child(
                        div().w(px(280.)).max_w_full().child(
                            Slider::new(
                                self.element_id(id),
                                current as f32,
                                *min as f32..=*max as f32,
                                step,
                            )
                            .on_change(move |value, _, cx| {
                                let value = f64::from(*value);
                                let value = if whole {
                                    Value::from(value.round() as i64)
                                } else {
                                    Value::from(value)
                                };
                                on_edit(&pointer, value, cx)
                            }),
                        ),
                    )
                    .into_any_element()
            }
        }
    }

    /// A checkbox row: a box that is ticked when `on`, and its label. A press writes
    /// `next` at `pointer`.
    fn check(&self, key: &str, label: String, on: bool, pointer: &str, next: Value) -> AnyElement {
        let on_edit = self.control.on_edit.clone();
        let pointer = pointer.to_owned();
        let state = format!("{label}, {}", if on { "checked" } else { "not checked" });
        let button = Button::new(gpui::ElementId::Name(self.element_id(key)), label)
            .icon(if on {
                IconName::Check
            } else {
                IconName::Square
            })
            .color(if on { Color::Default } else { Color::Muted })
            .aria_label(state)
            .debug_selector({
                let id = format!(
                    "{}-{key}-{}",
                    self.control.selector,
                    if on { "checked" } else { "unchecked" }
                );
                move || id
            })
            .on_click(move |_, _, cx| on_edit(&pointer, next.clone(), cx));
        // As wide as its label, not the column.
        h_flex().child(button).into_any_element()
    }
}
