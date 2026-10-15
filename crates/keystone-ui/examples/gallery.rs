//! Every keystone-ui component in every state, in a real window.
//! Run with `cargo run -p keystone-ui --example gallery`.

use gpui::{
    AnyElement, App, Application, Bounds, Context, Corner, IntoElement, Render, SharedString,
    Window, WindowBounds, WindowOptions, div, prelude::*, px, size,
};
use keystone_ui::prelude::*;

struct Gallery {
    menu_open: bool,
    selected_row: usize,
}

fn section(title: &'static str, body: impl IntoElement) -> impl IntoElement {
    v_flex()
        .gap_3()
        .child(
            Label::new(title)
                .size(TextSize::Sm)
                .color(Color::Faint)
                .weight(gpui::FontWeight::MEDIUM),
        )
        .child(body)
}

fn row(items: impl IntoIterator<Item = AnyElement>) -> impl IntoElement {
    h_flex().gap_3().flex_wrap().children(items)
}

impl Render for Gallery {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (background, text, ui_font) = (
            theme.colors.background,
            theme.colors.text,
            theme.fonts.ui.clone(),
        );
        let styles = [
            (ButtonStyle::Ghost, "Ghost"),
            (ButtonStyle::Subtle, "Subtle"),
            (ButtonStyle::Filled, "Filled"),
        ];
        let sizes = [
            (ButtonSize::Compact, "Compact"),
            (ButtonSize::Default, "Default"),
            (ButtonSize::Large, "Large"),
        ];
        let buttons = v_flex()
            .gap_2()
            .children(styles.into_iter().map(|(style, name)| {
                row(sizes
                    .into_iter()
                    .map(move |(size, size_name)| {
                        Button::new(
                            SharedString::from(format!("{name}-{size_name}")),
                            format!("{name} {size_name}"),
                        )
                        .style(style)
                        .size(size)
                        .icon(IconName::Plus)
                        .tooltip(Tooltip::with_key(format!("{name} button"), "Ctrl+B"))
                        .on_click(|_, _, _| {})
                        .into_any_element()
                    })
                    .chain([
                        Button::new(SharedString::from(format!("{name}-selected")), "Selected")
                            .style(style)
                            .selected(true)
                            .into_any_element(),
                        Button::new(SharedString::from(format!("{name}-disabled")), "Disabled")
                            .style(style)
                            .disabled(true)
                            .into_any_element(),
                    ]))
            }));
        let icon_buttons = row(sizes.into_iter().flat_map(|(size, size_name)| {
            [
                IconButton::new(
                    SharedString::from(format!("icon-{size_name}")),
                    IconName::Search,
                )
                .size(size)
                .tooltip(Tooltip::with_key("Search", "Ctrl+F"))
                .on_click(|_, _, _| {})
                .into_any_element(),
                IconButton::new(
                    SharedString::from(format!("icon-sel-{size_name}")),
                    IconName::Inbox,
                )
                .size(size)
                .selected(true)
                .into_any_element(),
                IconButton::new(
                    SharedString::from(format!("icon-dis-{size_name}")),
                    IconName::Close,
                )
                .size(size)
                .disabled(true)
                .into_any_element(),
            ]
        }));
        let icons = row(IconName::ALL
            .iter()
            .map(|&icon| Icon::new(icon).into_any_element()));
        let labels = v_flex()
            .gap_2()
            .child(row(TextSize::ALL.into_iter().map(|size| {
                Label::new(format!("{size:?}"))
                    .size(size)
                    .into_any_element()
            })))
            .child(row([
                Color::Default,
                Color::Muted,
                Color::Faint,
                Color::Placeholder,
                Color::Accent,
                Color::Danger,
                Color::Success,
                Color::Warning,
            ]
            .into_iter()
            .map(|color| {
                Label::new(format!("{color:?}"))
                    .color(color)
                    .into_any_element()
            })))
            .child(row([
                Label::new("cargo run -p keystone-desktop")
                    .mono()
                    .into_any_element(),
                Kbd::new("Ctrl+Shift+N").into_any_element(),
                div()
                    .w(px(160.))
                    .child(Label::new("A long session title that truncates").truncate())
                    .into_any_element(),
            ]));
        let list = v_flex().w(px(256.)).gap(px(2.)).children(
            [
                "Refactor storage layer",
                "Investigate rapid-input stall",
                "Settled session",
            ]
            .into_iter()
            .enumerate()
            .map(|(index, title)| {
                ListItem::new(("row", index))
                    .selected(index == self.selected_row)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected_row = index;
                        cx.notify();
                    }))
                    .child(Label::new(title).truncate())
                    .child(
                        Label::new("Claude · Draft")
                            .size(TextSize::Sm)
                            .color(Color::Muted),
                    )
            }),
        );
        let menu = PopoverMenu::new(
            "gallery-menu",
            Button::new("menu-trigger", "Open menu")
                .style(ButtonStyle::Subtle)
                .icon(IconName::Chevron)
                .selected(self.menu_open),
        )
        .open(self.menu_open)
        .anchor(Corner::TopLeft)
        .on_open_change(cx.listener(|this, open: &bool, _, cx| {
            this.menu_open = *open;
            cx.notify();
        }))
        .child(
            MenuItem::new("claude", "New Claude session")
                .icon(IconName::Chat)
                .kbd("Ctrl+N"),
        )
        .child(
            MenuItem::new("codex", "New Codex session")
                .icon(IconName::Chat)
                .kbd("Ctrl+Shift+N"),
        )
        .child(MenuSeparator)
        .child(MenuItem::new("settings", "Settings").icon(IconName::Settings));
        let surfaces = row([Elevation::Surface, Elevation::Raised, Elevation::Overlay]
            .into_iter()
            .map(|elevation| {
                div()
                    .w(px(160.))
                    .h(px(72.))
                    .p_3()
                    .surface(elevation, cx)
                    .child(Label::new(format!("{elevation:?}")).size(TextSize::Ui))
                    .into_any_element()
            }));
        div()
            .id("gallery")
            .size_full()
            .overflow_y_scroll()
            .bg(background)
            .font_family(ui_font)
            .text_color(text)
            .text_size(TextSize::Base.px())
            .child(
                v_flex()
                    .p_6()
                    .gap_8()
                    .child(Label::new("keystone-ui").size(TextSize::Title))
                    .child(section("BUTTONS", buttons))
                    .child(section("ICON BUTTONS", icon_buttons))
                    .child(section(
                        "POPOVER MENU",
                        h_flex().h(px(150.)).items_start().child(menu),
                    ))
                    .child(section("LIST ITEMS", list))
                    .child(section("LABELS", labels))
                    .child(section("ICONS", icons))
                    .child(section("SURFACES", surfaces)),
            )
    }
}

fn main() {
    Application::new()
        .with_assets(keystone_ui::Icons)
        .run(|cx: &mut App| {
            keystone_ui::init(cx);
            let bounds = Bounds::centered(None, size(px(1100.), px(900.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |_, cx| {
                    cx.new(|_| Gallery {
                        menu_open: true,
                        selected_row: 0,
                    })
                },
            )
            .expect("open gallery window");
            cx.activate(true);
        });
}
