use crate::button::ClickHandler;
use crate::theme::ActiveTheme;
use crate::tokens::radius;
use gpui::{
    AnyElement, App, ClickEvent, ElementId, IntoElement, ParentElement, RenderOnce, Styled, Window,
    div, prelude::*, px,
};
use smallvec::SmallVec;

type HoverHandler = Box<dyn Fn(&bool, &mut Window, &mut App) + 'static>;

/// A selectable row, such as a session in the sidebar.
#[derive(IntoElement)]
pub struct ListItem {
    id: ElementId,
    selected: bool,
    on_hover: Option<HoverHandler>,
    on_click: Option<ClickHandler>,
    debug_selector: Option<String>,
    children: SmallVec<[AnyElement; 2]>,
}

impl ListItem {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            selected: false,
            on_hover: None,
            on_click: None,
            debug_selector: None,
            children: SmallVec::new(),
        }
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    /// Called when the pointer enters (`true`) or leaves (`false`) the row.
    pub fn on_hover(mut self, handler: impl Fn(&bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_hover = Some(Box::new(handler));
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

impl ParentElement for ListItem {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements)
    }
}

impl RenderOnce for ListItem {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let colors = &theme.colors;
        let (selected_bg, hover_bg, active_bg) = (
            colors.element_selected,
            colors.element_hover,
            colors.element_active,
        );
        let mut el = div()
            .id(self.id)
            .flex()
            .flex_col()
            .min_w_0()
            .px(px(10.))
            .py(theme.density.pad(px(7.)))
            .gap(px(3.))
            .rounded(radius::ROW)
            .cursor_pointer()
            .when(self.selected, |d| {
                d.bg(selected_bg).hover(move |s| s.bg(active_bg))
            })
            .when(!self.selected, |d| d.hover(move |s| s.bg(hover_bg)))
            .children(self.children);
        if let Some(selector) = self.debug_selector {
            el = crate::automation::tag(el, selector);
        }
        el.when_some(self.on_click, |d, handler| d.on_click(handler))
            .when_some(self.on_hover, |d, handler| d.on_hover(handler))
    }
}
