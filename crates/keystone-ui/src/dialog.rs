use crate::button::{Button, ButtonStyle};
use crate::color::Color;
use crate::icon::{Icon, IconName};
use crate::label::Label;
use crate::styled_ext::{StyledExt, h_flex, v_flex};
use crate::theme::ActiveTheme;
use crate::tokens::{Elevation, TextSize, spacing};
use gpui::{
    AnchoredPositionMode, App, FocusHandle, FontWeight, IntoElement, KeyDownEvent, RenderOnce,
    SharedString, Window, anchored, deferred, div, point, prelude::*, px,
};
use std::rc::Rc;

type Handler = Rc<dyn Fn(&mut Window, &mut App) + 'static>;

/// Keyboard focus for a [`ConfirmDialog`]'s two buttons. Create it when the dialog
/// opens and keep it while the dialog shows. It focuses Cancel, so Enter alone
/// never confirms.
#[derive(Clone)]
pub struct DialogFocus {
    cancel: FocusHandle,
    confirm: FocusHandle,
}

impl DialogFocus {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        let focus = Self {
            cancel: cx.focus_handle(),
            confirm: cx.focus_handle(),
        };
        window.focus(&focus.cancel, cx);
        focus
    }
}

/// A modal question over the whole window, with Cancel and Confirm. It draws above
/// everything else, menus included, and a scrim takes every press outside the card.
///
/// Cancel, Escape, and a press on the scrim call `on_cancel`; only Confirm calls
/// `on_confirm`. Tab moves between the buttons, and Enter or Space presses the
/// focused one. The card's key context is [`ConfirmDialog::KEY_CONTEXT`], so app
/// shortcuts bound with `!Dialog` stay off while it is open.
///
/// The dialog is `id`, its buttons `{id}-cancel` and `{id}-confirm`, for tests and
/// native verification.
#[derive(IntoElement)]
pub struct ConfirmDialog {
    id: SharedString,
    focus: DialogFocus,
    title: SharedString,
    message: SharedString,
    tone: Color,
    on_cancel: Option<Handler>,
    on_confirm: Option<Handler>,
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
            tone: Color::Danger,
            on_cancel: None,
            on_confirm: None,
        }
    }

    /// The color of the icon and of Confirm. `Danger` by default; `Warning` for a
    /// choice that is risky but reversible.
    pub fn tone(mut self, tone: Color) -> Self {
        self.tone = tone;
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
        let DialogFocus {
            cancel: cancel_focus,
            confirm: confirm_focus,
        } = self.focus;
        let keys = {
            let on_cancel = on_cancel.clone();
            let (cancel_focus, confirm_focus) = (cancel_focus.clone(), confirm_focus.clone());
            move |event: &KeyDownEvent, window: &mut Window, cx: &mut App| {
                match event.keystroke.key.as_str() {
                    "escape" => on_cancel(window, cx),
                    // Two buttons, so Tab and Shift+Tab both move to the other one.
                    "tab" if confirm_focus.is_focused(window) => window.focus(&cancel_focus, cx),
                    "tab" => window.focus(&confirm_focus, cx),
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
                    .child(Icon::new(IconName::Shield).size(px(16.)).color(self.tone))
                    .child(
                        Label::new(self.title)
                            .size(TextSize::Body)
                            .weight(FontWeight::MEDIUM),
                    ),
            )
            .child(Label::new(self.message).color(Color::Muted))
            .child(
                h_flex()
                    .pt(spacing::XS)
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new(SharedString::from(format!("{id}-cancel")), "Cancel")
                            .style(ButtonStyle::Subtle)
                            .track_focus(&cancel_focus)
                            .debug_selector(|| format!("{id}-cancel"))
                            .on_click({
                                let on_cancel = on_cancel.clone();
                                move |_, window, cx| on_cancel(window, cx)
                            }),
                    )
                    .child(
                        Button::new(SharedString::from(format!("{id}-confirm")), "Confirm")
                            .style(ButtonStyle::Filled)
                            .color(self.tone)
                            .track_focus(&confirm_focus)
                            .debug_selector(|| format!("{id}-confirm"))
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
            .on_click(move |_, window, cx| on_cancel(window, cx))
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
