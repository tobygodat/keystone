use crate::button::ClickHandler;
use crate::icon::{Icon, IconName};
use crate::kbd::Kbd;
use crate::label::Label;
use crate::styled_ext::{StyledExt, h_flex, v_flex};
use crate::theme::ActiveTheme;
use crate::tokens::{Elevation, TextSize, radius};
use gpui::{
    AnyElement, App, ClickEvent, Corner, ElementId, IntoElement, MouseButton, ParentElement,
    RenderOnce, SharedString, Styled, Window, anchored, deferred, div, prelude::*, px,
};
use smallvec::SmallVec;
use std::rc::Rc;

type OpenHandler = Rc<dyn Fn(&bool, &mut Window, &mut App) + 'static>;

/// A trigger with an anchored overlay menu. The caller owns `open` and updates it from
/// `on_open_change`, which fires `true` when the trigger is pressed while closed and
/// `false` on any press outside the menu, including the trigger itself.
#[derive(IntoElement)]
pub struct PopoverMenu {
    id: ElementId,
    trigger: AnyElement,
    open: bool,
    anchor: Corner,
    on_open_change: Option<OpenHandler>,
    children: SmallVec<[AnyElement; 4]>,
}

impl PopoverMenu {
    pub fn new(id: impl Into<ElementId>, trigger: impl IntoElement) -> Self {
        Self {
            id: id.into(),
            trigger: trigger.into_any_element(),
            open: false,
            anchor: Corner::TopLeft,
            on_open_change: None,
            children: SmallVec::new(),
        }
    }

    pub fn open(mut self, open: bool) -> Self {
        self.open = open;
        self
    }

    /// The menu corner pinned to the matching trigger corner below it. `TopRight` opens
    /// down and aligns right edges.
    pub fn anchor(mut self, anchor: Corner) -> Self {
        self.anchor = anchor;
        self
    }

    pub fn on_open_change(
        mut self,
        handler: impl Fn(&bool, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_open_change = Some(Rc::new(handler));
        self
    }
}

impl ParentElement for PopoverMenu {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements)
    }
}

impl RenderOnce for PopoverMenu {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let open = self.open;
        let handler = self.on_open_change;
        // Opening on mouse down, not click: the menu's outside-press handler closes it in
        // the capture phase of the same press, so a click-based toggle would reopen it.
        let trigger = div()
            .when_some(handler.clone().filter(|_| !open), |d, handler| {
                d.on_mouse_down(MouseButton::Left, move |_, window, cx| {
                    handler(&true, window, cx)
                })
            })
            .child(self.trigger);
        let menu = open.then(|| {
            let card = v_flex()
                .id(self.id)
                .occlude()
                .min_w(px(220.))
                .p(px(4.))
                .gap(px(2.))
                .surface(Elevation::Overlay, cx)
                .when_some(handler, |d, handler| {
                    d.on_mouse_down_out(move |_, window, cx| handler(&false, window, cx))
                })
                .children(self.children);
            let pin = div().absolute().size_0();
            let pin = match self.anchor {
                Corner::TopLeft => pin.bottom_0().left_0(),
                Corner::TopRight => pin.bottom_0().right_0(),
                Corner::BottomLeft => pin.top_0().left_0(),
                Corner::BottomRight => pin.top_0().right_0(),
            };
            let gap = match self.anchor {
                Corner::TopLeft | Corner::TopRight => div().pt(px(4.)),
                Corner::BottomLeft | Corner::BottomRight => div().pb(px(4.)),
            };
            pin.child(
                deferred(
                    anchored()
                        .anchor(self.anchor)
                        .snap_to_window_with_margin(px(8.))
                        .child(gap.child(card)),
                )
                .priority(1),
            )
        });
        div().relative().child(trigger).children(menu)
    }
}

/// One row in a `PopoverMenu`.
#[derive(IntoElement)]
pub struct MenuItem {
    id: ElementId,
    label: SharedString,
    icon: Option<IconName>,
    kbd: Option<SharedString>,
    on_click: Option<ClickHandler>,
    debug_selector: Option<String>,
}

impl MenuItem {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon: None,
            kbd: None,
            on_click: None,
            debug_selector: None,
        }
    }

    pub fn icon(mut self, icon: IconName) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn kbd(mut self, keys: impl Into<SharedString>) -> Self {
        self.kbd = Some(keys.into());
        self
    }

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }

    pub fn debug_selector(mut self, selector: impl FnOnce() -> String) -> Self {
        self.debug_selector = Some(selector());
        self
    }
}

impl RenderOnce for MenuItem {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let hover = cx.theme().colors.element_hover;
        let mut el = h_flex()
            .id(self.id)
            .h(px(28.))
            .px(px(8.))
            .gap_2()
            .rounded(radius::MENU_ROW)
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .when_some(self.icon, |d, icon| d.child(Icon::new(icon).size(px(14.))))
            .child(
                div()
                    .flex_1()
                    .child(Label::new(self.label).size(TextSize::Base)),
            )
            .when_some(self.kbd, |d, keys| d.child(Kbd::new(keys)));
        if let Some(selector) = self.debug_selector {
            el = el.debug_selector(move || selector);
        }
        el.when_some(self.on_click, |d, handler| {
            d.on_click(move |event, window, cx| {
                cx.stop_propagation();
                handler(event, window, cx)
            })
        })
    }
}

/// A hairline between groups of menu rows.
#[derive(IntoElement)]
pub struct MenuSeparator;

impl RenderOnce for MenuSeparator {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        div()
            .mx(px(-4.))
            .my(px(2.))
            .h(px(1.))
            .bg(cx.theme().colors.border)
    }
}
