//! The tab bar page: eight tabs to select, close, and drag, and a button that adds more.

use gpui::{Context, IntoElement, Render, Window, div, prelude::*};
use keystone_ui::prelude::*;

pub struct TabsPage {
    /// Tab numbers in display order. A number is the tab's id and its label.
    order: Vec<usize>,
    selected: usize,
    next: usize,
}

impl TabsPage {
    pub fn new() -> Self {
        Self {
            order: (1..=8).collect(),
            selected: 0,
            next: 9,
        }
    }
}

impl Render for TabsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tabs = self.order.iter().map(|&n| {
            let dot = match n {
                2 => Some(TabDot::Working),
                4 => Some(TabDot::Dirty),
                _ => None,
            };
            Tab::new(("gallery-tab", n), format!("Session {n}")).dot(dot)
        });
        let order = self
            .order
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        v_flex()
            .gap_2()
            .child(
                TabBar::new("tabs")
                    .children(tabs)
                    .selected(self.selected)
                    .on_select(cx.listener(|this, &ix: &usize, _, cx| {
                        this.selected = ix;
                        cx.notify();
                    }))
                    .on_close(cx.listener(|this, &ix: &usize, _, cx| {
                        this.order.remove(ix);
                        if this.selected > ix || this.selected == this.order.len() {
                            this.selected = this.selected.saturating_sub(1);
                        }
                        cx.notify();
                    }))
                    .on_move(cx.listener(|this, &(from, to): &(usize, usize), _, cx| {
                        let selected = this.order[this.selected];
                        let tab = this.order.remove(from);
                        this.order.insert(to, tab);
                        this.selected = this.order.iter().position(|&n| n == selected).unwrap_or(0);
                        cx.notify();
                    })),
            )
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        Button::new("tabs-add", "Add tab")
                            .style(ButtonStyle::Subtle)
                            .icon(IconName::Plus)
                            .debug_selector(|| "tabs-add".into())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.order.push(this.next);
                                this.next += 1;
                                this.selected = this.order.len() - 1;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .automation_id("tabs-order")
                            .child(Label::new(format!("Order: {order}")).color(Color::Muted)),
                    ),
            )
    }
}
