use crate::button::{Button, ButtonStyle};
use crate::color::Color;
use crate::icon::{Icon, IconName};
use crate::label::Label;
use crate::styled_ext::{StyledExt, h_flex, v_flex};
use crate::theme::ActiveTheme;
use crate::tokens::{Elevation, TextSize, spacing};
use gpui::{
    AnchoredPositionMode, AnyElement, App, FocusHandle, FontWeight, IntoElement, KeyDownEvent,
    RenderOnce, SharedString, Window, anchored, deferred, div, point, prelude::*, px,
};
use std::rc::Rc;

type Handler = Rc<dyn Fn(&mut Window, &mut App) + 'static>;

/// Keyboard focus for a [`ConfirmDialog`]: its buttons and, for a dialog with a
/// field, that field. Create it when the dialog opens and keep it while the dialog
/// shows.
#[derive(Clone)]
pub struct DialogFocus {
    body: Option<FocusHandle>,
    cancel: FocusHandle,
    confirm: FocusHandle,
}

impl DialogFocus {
    /// Focuses Cancel, so Enter alone never confirms.
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        let focus = Self::with_stops(None, cx);
        window.focus(&focus.cancel, cx);
        focus
    }

    /// For a dialog whose body holds a field: focuses `body`, and Tab cycles the
    /// field, Cancel, and Confirm.
    pub fn with_body(body: FocusHandle, window: &mut Window, cx: &mut App) -> Self {
        window.focus(&body, cx);
        Self::with_stops(Some(body), cx)
    }

    fn with_stops(body: Option<FocusHandle>, cx: &mut App) -> Self {
        Self {
            body,
            cancel: cx.focus_handle(),
            confirm: cx.focus_handle(),
        }
    }

    fn cycle(&self, backward: bool, window: &mut Window, cx: &mut App) {
        let stops: Vec<&FocusHandle> = self
            .body
            .iter()
            .chain([&self.cancel, &self.confirm])
            .collect();
        let next = match stops
            .iter()
            .position(|stop| stop.contains_focused(window, cx))
        {
            Some(at) if backward => (at + stops.len() - 1) % stops.len(),
            Some(at) => (at + 1) % stops.len(),
            None if backward => stops.len() - 1,
            None => 0,
        };
        window.focus(stops[next], cx);
    }
}

/// A modal question over the whole window, with Cancel and Confirm. It draws above
/// everything else, menus included, and a scrim takes every press outside the card.
///
/// Cancel, Escape, and a press on the scrim call `on_cancel`; only Confirm calls
/// `on_confirm`. While [`ConfirmDialog::working`], Confirm is off and a press on the
/// scrim does nothing, so only Cancel and Escape stop the work. Tab and Shift+Tab cycle the stops of [`DialogFocus`], and Enter or
/// Space presses the focused button. The card's key context is
/// [`ConfirmDialog::KEY_CONTEXT`], so app shortcuts bound with `!Dialog` stay off
/// while it is open.
///
/// The dialog is `id`, its buttons `{id}-cancel` and `{id}-confirm`, for tests and
/// native verification.
#[derive(IntoElement)]
pub struct ConfirmDialog {
    id: SharedString,
    focus: DialogFocus,
    title: SharedString,
    message: SharedString,
    body: Option<AnyElement>,
    icon: IconName,
    confirm_label: SharedString,
    tone: Color,
    on_cancel: Option<Handler>,
    on_confirm: Option<Handler>,
    working: bool,
}

impl ConfirmDialog {
    pub const KEY_CONTEXT: &str = "Dialog";

    pub fn new(
        id: impl Into<SharedString>,
        focus: &DialogFocus,
        title: impl Into<SharedString>,
        message: impl Into<SharedString>,
    ) -> Self {
        Self {
            id: id.into(),
            focus: focus.clone(),
            title: title.into(),
            message: message.into(),
            body: None,
            icon: IconName::Shield,
            confirm_label: "Confirm".into(),
            tone: Color::Danger,
            on_cancel: None,
            on_confirm: None,
            working: false,
        }
    }

    /// The color of the icon and of Confirm. `Danger` by default; `Warning` for a
    /// choice that is risky but reversible. `Default` is a safe choice: an ink icon and
    /// Confirm as the primary action.
    pub fn tone(mut self, tone: Color) -> Self {
        self.tone = tone;
        self
    }

    /// Drawn after the message, such as the field a dialog asks the user to fill in.
    pub fn body(mut self, body: impl IntoElement) -> Self {
        self.body = Some(body.into_any_element());
        self
    }

    /// The icon before the title. A shield by default.
    pub fn icon(mut self, icon: IconName) -> Self {
        self.icon = icon;
        self
    }

    /// The confirm button's text. "Confirm" by default.
    pub fn confirm_label(mut self, label: impl Into<SharedString>) -> Self {
        self.confirm_label = label.into();
        self
    }

    /// Whether what Confirm started is still running.
    pub fn working(mut self, working: bool) -> Self {
        self.working = working;
        self
    }

    pub fn on_cancel(mut self, handler: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_cancel = Some(Rc::new(handler));
        self
    }

    pub fn on_confirm(mut self, handler: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_confirm = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for ConfirmDialog {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let call = |handler: &Option<Handler>| {
            let handler = handler.clone();
            move |window: &mut Window, cx: &mut App| {
                if let Some(handler) = &handler {
                    handler(window, cx)
                }
            }
        };
        let on_cancel = call(&self.on_cancel);
        let on_confirm = call(&self.on_confirm);
        let id = self.id;
        let focus = self.focus;
        let keys = {
            let on_cancel = on_cancel.clone();
            let focus = focus.clone();
            move |event: &KeyDownEvent, window: &mut Window, cx: &mut App| {
                match event.keystroke.key.as_str() {
                    "escape" => on_cancel(window, cx),
                    "tab" => focus.cycle(event.keystroke.modifiers.shift, window, cx),
                    _ => return,
                }
                cx.stop_propagation();
            }
        };
        let card = v_flex()
            .id(SharedString::from(format!("{id}-card")))
            .key_context(Self::KEY_CONTEXT)
            .occlude()
            .w(px(400.))
            .max_w_full()
            .p(spacing::LG)
            .gap(spacing::MD)
            .surface(Elevation::Overlay, cx)
            .on_key_down(keys)
            .child(
                h_flex()
                    .gap_2()
                    .child(Icon::new(self.icon).size(px(16.)).color(self.tone))
                    .child(
                        Label::new(self.title)
                            .size(TextSize::Body)
                            .weight(FontWeight::MEDIUM),
                    ),
            )
            .when(!self.message.is_empty(), |d| {
                d.child(Label::new(self.message).color(Color::Muted))
            })
            .children(self.body)
            .child(
                h_flex()
                    .pt(spacing::XS)
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new(SharedString::from(format!("{id}-cancel")), "Cancel")
                            .style(ButtonStyle::Subtle)
                            .track_focus(&focus.cancel)
                            .debug_selector(|| format!("{id}-cancel"))
                            .on_click({
                                let on_cancel = on_cancel.clone();
                                move |_, window, cx| on_cancel(window, cx)
                            }),
                    )
                    .child(
                        Button::new(
                            SharedString::from(format!("{id}-confirm")),
                            self.confirm_label,
                        )
                        .map(|button| {
                            if self.tone == Color::Default {
                                button.style(ButtonStyle::Primary)
                            } else {
                                button.style(ButtonStyle::Filled).color(self.tone)
                            }
                        })
                        .track_focus(&focus.confirm)
                        .debug_selector(|| format!("{id}-confirm"))
                        .disabled(self.working)
                        .on_click(move |_, window, cx| on_confirm(window, cx)),
                    ),
            );
        let card = crate::automation::tag(card, id.to_string());
        let scrim = div()
            .id(SharedString::from(format!("{id}-scrim")))
            .w(window.viewport_size().width)
            .h(window.viewport_size().height)
            .occlude()
            .bg(cx.theme().colors.scrim)
            .flex()
            .items_center()
            .justify_center()
            .p(spacing::XL)
            .when(!self.working, |scrim| {
                scrim.on_click(move |_, window, cx| on_cancel(window, cx))
            })
            .child(card);
        deferred(
            anchored()
                .position_mode(AnchoredPositionMode::Window)
                .position(point(px(0.), px(0.)))
                .child(scrim),
        )
        .priority(2)
    }
}
