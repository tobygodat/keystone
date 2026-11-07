//! The controls page: switches, a slider, and selects, with a status line that names
//! the last change. Tab and Shift+Tab move focus between the controls.

use gpui::{
    Context, FocusHandle, Focusable, IntoElement, Render, SharedString, Window, div, prelude::*, px,
};
use keystone_ui::prelude::*;

const FRUITS: [&str; 12] = [
    "Apple",
    "Banana",
    "Cherry",
    "Mango",
    "Melon",
    "Nectarine",
    "Orange",
    "Peach",
    "Plum",
    "Quince",
    "Raspberry",
    "Strawberry",
];

pub struct ControlsPage {
    focus: FocusHandle,
    switch: bool,
    slider: f32,
    action: keystone_ui::ActionColor,
    fruit: Option<usize>,
    model: Option<usize>,
    models: Vec<SharedString>,
    status: SharedString,
}

impl ControlsPage {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
            switch: false,
            slider: 40.,
            action: keystone_ui::ActionColor::Ink,
            fruit: Some(0),
            model: None,
            models: (1..=500).map(|n| format!("Model {n:03}").into()).collect(),
            status: "No change yet".into(),
        }
    }
}

impl Focusable for ControlsPage {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

fn field(label: &'static str, control: impl IntoElement) -> impl IntoElement {
    h_flex()
        .gap_4()
        .child(
            div()
                .w(px(120.))
                .child(Label::new(label).color(Color::Muted)),
        )
        .child(control)
}

impl Render for ControlsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("controls-page")
            .track_focus(&self.focus)
            .on_key_down(|event, window, cx| {
                if event.keystroke.key == "tab" {
                    if event.keystroke.modifiers.shift {
                        window.focus_prev(cx);
                    } else {
                        window.focus_next(cx);
                    }
                    cx.stop_propagation();
                }
            })
            .flex_1()
            .px_6()
            .pb_6()
            .gap_4()
            .child(
                div()
                    .automation_id("controls-status")
                    .child(Label::new(self.status.clone()).color(Color::Muted)),
            )
            .child(field(
                "Switch",
                Switch::new("controls-switch", self.switch)
                    .label(if self.switch { "On" } else { "Off" })
                    .on_change(cx.listener(|this, on: &bool, _, cx| {
                        this.switch = *on;
                        this.status = format!("switch {on}").into();
                        cx.notify();
                    })),
            ))
            .child(field(
                "Disabled",
                h_flex()
                    .gap_4()
                    .child(
                        Switch::new("controls-switch-disabled", false)
                            .disabled(true)
                            .on_change(cx.listener(|this, on: &bool, _, cx| {
                                this.status = format!("disabled switch {on}").into();
                                cx.notify();
                            })),
                    )
                    .child(Switch::new("controls-switch-disabled-on", true).disabled(true)),
            ))
            .child(field(
                "Slider",
                h_flex()
                    .gap_4()
                    .child(div().w(px(240.)).child(
                        Slider::new("controls-slider", self.slider, 0.0..=100.0, 5.).on_change(
                            cx.listener(|this, value: &f32, _, cx| {
                                this.slider = *value;
                                this.status = format!("slider {value}").into();
                                cx.notify();
                            }),
                        ),
                    ))
                    .child(
                        div()
                            .automation_id("controls-slider-value")
                            .child(Label::new(format!("{}", self.slider))),
                    ),
            ))
            .child(field(
                "Swatch",
                h_flex()
                    .gap_1()
                    .children(keystone_ui::ActionColor::ALL.map(|action| {
                        Swatch::new(action.name(), action.tint(cx.theme().appearance).fill)
                            .selected(self.action == action)
                            .tooltip(Tooltip::text(action.name()))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.action = action;
                                this.status = format!("swatch {}", action.name()).into();
                                cx.notify();
                            }))
                    })),
            ))
            .child(field(
                "Slider, disabled",
                div().w(px(240.)).child(
                    Slider::new("controls-slider-disabled", 70., 0.0..=100.0, 5.).disabled(true),
                ),
            ))
            .child(field(
                "Select",
                Select::new("controls-select", FRUITS, self.fruit).on_change(cx.listener(
                    |this, ix: &usize, _, cx| {
                        this.fruit = Some(*ix);
                        this.status = format!("select {}", FRUITS[*ix]).into();
                        cx.notify();
                    },
                )),
            ))
            .child(field(
                "500 options",
                Select::new("controls-models", self.models.clone(), self.model)
                    .placeholder("Choose a model")
                    .on_change(cx.listener(|this, ix: &usize, _, cx| {
                        this.model = Some(*ix);
                        this.status = format!("model {}", this.models[*ix]).into();
                        cx.notify();
                    })),
            ))
            .child(field(
                "Select, disabled",
                Select::new("controls-select-disabled", FRUITS, Some(2)).disabled(true),
            ))
    }
}
