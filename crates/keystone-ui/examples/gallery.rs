//! Every keystone-ui component in every state, in a real window.
//! Run with `cargo run -p keystone-ui --example gallery`.

use gpui::{
    Anchor, AnyElement, App, Bounds, Context, Entity, Focusable, IntoElement, Render, SharedString,
    Window, WindowBounds, WindowOptions, div, prelude::*, px, size,
};
use keystone_ui::prelude::*;
use keystone_ui::{Appearance, Density, Fonts, Theme, automation, set_theme};

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Components,
    List,
    Controls,
    Tree,
    Palette,
    Diagrams,
    Editor,
}

#[path = "gallery/controls.rs"]
mod controls;
#[path = "gallery/diagrams.rs"]
mod diagrams;
#[path = "gallery/editor.rs"]
mod editor;
#[path = "gallery/palette.rs"]
mod palette;
#[path = "gallery/tabs.rs"]
mod tabs;
#[path = "gallery/tree.rs"]
mod tree;

struct Gallery {
    focus: gpui::FocusHandle,
    page: Page,
    menu_open: bool,
    dialog: Option<DialogFocus>,
    selected_row: usize,
    markdown: std::sync::Arc<keystone_ui::markdown::Document>,
    split: Option<SplitPage>,
    tabs: gpui::Entity<tabs::TabsPage>,
    list: Entity<VirtualListState>,
    confirmed: Option<usize>,
    palette: Entity<palette::PalettePage>,
    controls: Entity<controls::ControlsPage>,
    tree: Entity<tree::TreePage>,
    diagrams: Entity<diagrams::DiagramsPage>,
    editor: Entity<editor::EditorPage>,
}

const MARKDOWN: &str = "## Heading\n\nA paragraph with **strong**, *emphasis*, `inline code`, ~~struck~~, and a [link](https://example.com).\n\n- A bullet\n  1. A nested number\n- Another bullet\n\n> A quoted line.\n\n| Left | Center | Right |\n| :-- | :-: | --: |\n| `a` | b | 1 |\n| c | d | 22 |\n\n```rust\nfn main() {\n    println!(\"A long line that scrolls sideways instead of wrapping, as in an editor, so code keeps its shape.\");\n}\n```\n\n---\n";

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

struct SplitPage {
    state: gpui::Entity<SplitState>,
    status: SharedString,
}

/// The payload of the split page's draggable tab.
struct GalleryTab;

struct TabPreview;

impl Render for TabPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_1()
            .surface(Elevation::Overlay, cx)
            .font_ui(cx)
            .child(Label::new("Tab").size(TextSize::Ui))
    }
}

impl Gallery {
    fn split_page(&self, page: &SplitPage, cx: &mut Context<Self>) -> impl IntoElement {
        let state = page.state.clone();
        let axis = state.read(cx).axis();
        let fractions = state.read(cx).fractions().to_vec();
        let view = cx.entity();
        let toolbar = h_flex()
            .gap_3()
            .p_3()
            .child(
                Button::new("split-close", "Back")
                    .style(ButtonStyle::Subtle)
                    .debug_selector(|| "split-close".into())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.split = None;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("split-axis", format!("{axis:?}"))
                    .style(ButtonStyle::Subtle)
                    .debug_selector(|| "split-axis".into())
                    .on_click({
                        let state = state.clone();
                        move |_, _, cx| {
                            state.update(cx, |state, cx| {
                                let axis = match state.axis() {
                                    gpui::Axis::Horizontal => gpui::Axis::Vertical,
                                    gpui::Axis::Vertical => gpui::Axis::Horizontal,
                                };
                                state.set_axis(axis);
                                cx.notify();
                            })
                        }
                    }),
            )
            .child(
                div()
                    .id("split-tab")
                    .automation_id("split-tab")
                    .px_3()
                    .py_1()
                    .surface(Elevation::Raised, cx)
                    .child(Label::new("Drag this tab onto a pane edge").size(TextSize::Ui))
                    .on_drag(GalleryTab, |_, _, _, cx| cx.new(|_| TabPreview)),
            )
            .child(
                div()
                    .id("split-status")
                    .automation_id("split-status")
                    .child(Label::new(page.status.clone()).color(Color::Muted)),
            );
        let split = Split::new("split", &state)
            .on_drop_edge(move |_: &GalleryTab, target, _, cx| {
                view.update(cx, |this, cx| {
                    if let Some(page) = &mut this.split {
                        let edge = format!("{:?}", target.edge).to_lowercase();
                        page.status = format!("drop {edge} pane {}", target.pane).into();
                        cx.notify();
                    }
                })
            })
            .children(fractions.iter().enumerate().map(|(n, fraction)| {
                v_flex()
                    .size_full()
                    .p_4()
                    .gap_2()
                    .child(Label::new(format!("Pane {n}")).size(TextSize::Body))
                    .child(
                        Label::new(format!("{:.0}%", fraction * 100.))
                            .size(TextSize::Sm)
                            .color(Color::Muted),
                    )
            }));
        v_flex()
            .id("split-page")
            .occlude()
            .absolute()
            .inset_0()
            .bg(cx.theme().colors.background)
            .when(keystone_ui::automation::enabled(), |d| {
                d.child(keystone_ui::automation::frame_writer())
            })
            .child(toolbar)
            .child(div().flex_1().hairline_t(cx).child(split))
    }
}

fn row(items: impl IntoIterator<Item = AnyElement>) -> impl IntoElement {
    h_flex().gap_3().flex_wrap().children(items)
}

impl Gallery {
    fn list_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.list.read(cx);
        let highlighted = state
            .highlighted()
            .map_or("none".into(), |ix| ix.to_string());
        let confirmed = self.confirmed.map_or("none".into(), |ix| ix.to_string());
        let status = format!(
            "{} rows · highlighted {highlighted} · confirmed {confirmed}",
            state.count()
        );
        v_flex()
            .flex_1()
            .min_h_0()
            .px_6()
            .pb_6()
            .gap_3()
            .child(
                div()
                    .automation_id("gallery-list-status")
                    .child(Label::new(status).color(Color::Muted)),
            )
            .child(
                VirtualList::new("gallery-list", &self.list, |ix, highlighted, _, _| {
                    MenuItem::new(("row", ix), format!("Row {ix}"))
                        .highlighted(highlighted)
                        .into_any_element()
                })
                .on_confirm(cx.listener(|this, ix: &usize, _, cx| {
                    this.confirmed = Some(*ix);
                    cx.notify();
                }))
                .flex_1()
                .min_h_0()
                .max_w(px(480.)),
            )
    }
}

impl Render for Gallery {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (background, text, theme_appearance, theme_density) = (
            theme.colors.background,
            theme.colors.text,
            theme.appearance,
            theme.density,
        );
        let styles = [
            (ButtonStyle::Ghost, "Ghost"),
            (ButtonStyle::Subtle, "Subtle"),
            (ButtonStyle::Filled, "Filled"),
            (ButtonStyle::Primary, "Primary"),
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
                        Button::new(SharedString::from(format!("{name}-warning")), "Warning")
                            .style(style)
                            .icon(IconName::Shield)
                            .color(Color::Warning)
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
                .tooltip(Tooltip::with_key("Search", "Ctrl+Shift+F"))
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
            ]))
            .child(row([
                "keystone",
                "docs",
                "2026-plans",
                "web",
                "api",
                "infra",
                "zed",
            ]
            .into_iter()
            .enumerate()
            .map(|(tint, name)| Monogram::new(name, tint).into_any_element())));
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
        .anchor(Anchor::TopLeft)
        .on_open_change(cx.listener(|this, open: &bool, _, cx| {
            this.menu_open = *open;
            cx.notify();
        }))
        .child(MenuItem::new("claude", "Claude Code").kbd("Ctrl+N"))
        .child(MenuItem::new("codex", "Codex").kbd("Ctrl+Shift+N"))
        .child(MenuSeparator)
        .child(
            MenuItem::new("ask", "Ask first")
                .detail("Asks before commands not known to be safe")
                .checked(true),
        )
        .child(MenuSeparator)
        .child(MenuHeader::new("Density"))
        .child(MenuItem::new("compact", "Compact").checked(false))
        .child(MenuItem::new("default", "Default").checked(true))
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
        let dialog = h_flex().gap_3().child(
            Button::new("dialog-open", "Open confirm dialog")
                .style(ButtonStyle::Subtle)
                .icon(IconName::Shield)
                .debug_selector(|| "dialog-open".into())
                .on_click(cx.listener(|this, _, window, cx| {
                    this.dialog = Some(DialogFocus::new(window, cx));
                    cx.notify();
                })),
        );
        let close = cx.listener(|this: &mut Self, _: &(), _, cx| {
            this.dialog = None;
            cx.notify();
        });
        let close = std::rc::Rc::new(close);
        let page = self.page;
        let pages = [
            (Page::Components, "Components", "gallery-page-components"),
            (Page::List, "List", "gallery-page-list"),
            (Page::Palette, "Palette", "gallery-page-palette"),
            (Page::Controls, "Controls", "gallery-page-controls"),
            (Page::Tree, "Tree", "gallery-page-tree"),
            (Page::Diagrams, "Diagrams", "gallery-page-diagrams"),
            (Page::Editor, "Editor", "gallery-page-editor"),
        ]
        .map(|(target, name, id)| {
            Button::new(id, name)
                .style(ButtonStyle::Subtle)
                .selected(page == target)
                .debug_selector(|| id.into())
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.page = target;
                    if target == Page::List {
                        window.focus(&this.list.focus_handle(cx), cx);
                    }
                    if target == Page::Controls {
                        window.focus(&this.controls.focus_handle(cx), cx);
                    }
                    if target == Page::Tree {
                        window.focus(&this.tree.focus_handle(cx), cx);
                    }
                    if target == Page::Diagrams {
                        window.focus(&this.focus, cx);
                    }
                    if target == Page::Editor {
                        window.focus(&this.editor.focus_handle(cx), cx);
                    }
                    cx.notify();
                }))
        });
        let header = h_flex()
            .flex_wrap()
            .gap_3()
            .p_6()
            .child(Label::new("keystone-ui").size(TextSize::Title))
            .children(pages)
            .child(
                Button::new("split-open", "Split")
                    .style(ButtonStyle::Subtle)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.split = Some(SplitPage {
                            state: cx.new(|_| SplitState::new(gpui::Axis::Horizontal, 3)),
                            status: "No drop yet".into(),
                        });
                        cx.notify();
                    })),
            )
            .child(div().flex_1())
            .children([Appearance::Dark, Appearance::Light].map(|appearance| {
                Button::new(
                    SharedString::from(format!("{appearance:?}")),
                    format!("{appearance:?}"),
                )
                .style(ButtonStyle::Subtle)
                .selected(theme_appearance == appearance)
                .on_click(move |_, _, cx| {
                    let density = cx.theme().density;
                    set_theme(Theme::new(appearance, density, Fonts::default()), cx)
                })
            }))
            .children(Density::ALL.map(|density| {
                Button::new(
                    SharedString::from(format!("{density:?}")),
                    format!("{density:?}"),
                )
                .style(ButtonStyle::Subtle)
                .selected(theme_density == density)
                .on_click(move |_, _, cx| {
                    let appearance = cx.theme().appearance;
                    set_theme(Theme::new(appearance, density, Fonts::default()), cx)
                })
            }));
        let components = div()
            .id("gallery-components")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(
                v_flex()
                    .px_6()
                    .pb_6()
                    .gap_8()
                    .child(section("TAB BAR", self.tabs.clone()))
                    .child(section("BUTTONS", buttons))
                    .child(section("CONFIRM DIALOG", dialog))
                    .child(section("ICON BUTTONS", icon_buttons))
                    .child(section(
                        "POPOVER MENU",
                        h_flex().h(px(330.)).items_start().child(menu),
                    ))
                    .child(section(
                        "MARKDOWN",
                        div()
                            .max_w(px(760.))
                            .text_size(TextSize::Body.px())
                            .line_height(px(22.))
                            .child(Markdown::new("gallery-markdown", self.markdown.clone())),
                    ))
                    .child(section("LIST ITEMS", list))
                    .child(section("LABELS", labels))
                    .child(section("ICONS", icons))
                    .child(section("SURFACES", surfaces)),
            );
        v_flex()
            .id("gallery")
            .size_full()
            .bg(background)
            .font_ui(cx)
            .text_color(text)
            .text_size(TextSize::Base.px())
            .track_focus(&self.focus)
            .on_key_down(|event, window, cx| {
                if event.keystroke.key == "tab" {
                    if event.keystroke.modifiers.shift {
                        window.focus_prev(cx);
                    } else {
                        window.focus_next(cx);
                    }
                    cx.stop_propagation();
                } else if event.keystroke.key == "q" && event.keystroke.modifiers.secondary() {
                    cx.quit();
                }
            })
            .when(automation::enabled(), |d| {
                d.child(automation::frame_writer())
            })
            .child(header)
            .map(|d| match page {
                Page::Components => d.child(components),
                Page::List => d.child(self.list_page(cx)),
                Page::Palette => d.child(self.palette.clone()),
                Page::Controls => d.child(self.controls.clone()),
                Page::Tree => d.child(self.tree.clone()),
                Page::Diagrams => d.child(self.diagrams.clone()),
                Page::Editor => d.child(self.editor.clone()),
            })
            .when_some(self.split.as_ref(), |d, page| {
                d.child(self.split_page(page, cx))
            })
            .when_some(self.dialog.as_ref(), |d, focus| {
                let (cancel, confirm) = (close.clone(), close);
                d.child(
                    ConfirmDialog::new(
                        "gallery-dialog",
                        focus,
                        "Turn on Full access?",
                        "Codex will run commands and edit files without asking.",
                    )
                    .tone(Color::Warning)
                    .on_cancel(move |window, cx| cancel(&(), window, cx))
                    .on_confirm(move |window, cx| confirm(&(), window, cx)),
                )
            })
    }
}

fn main() {
    gpui_platform::application()
        .with_assets(keystone_ui::Icons)
        .run(|cx: &mut App| {
            keystone_ui::init(cx);
            TextInput::bind_keys(cx);
            automation::serve_inbox(cx);
            let bounds = Bounds::centered(None, size(px(1100.), px(900.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |window, cx| {
                    cx.new(|cx| {
                        let focus = cx.focus_handle();
                        window.focus(&focus, cx);
                        Gallery {
                            focus,
                            page: Page::Components,
                            menu_open: true,
                            dialog: None,
                            selected_row: 0,
                            markdown: std::sync::Arc::new(keystone_ui::markdown::Document::parse(
                                MARKDOWN,
                            )),
                            split: None,
                            tabs: cx.new(|_| tabs::TabsPage::new()),
                            list: cx.new(|cx| VirtualListState::new(50_000, cx)),
                            confirmed: None,
                            palette: cx.new(|_| palette::PalettePage::new()),
                            controls: cx.new(controls::ControlsPage::new),
                            tree: cx.new(tree::TreePage::new),
                            diagrams: cx.new(diagrams::DiagramsPage::new),
                            editor: cx.new(editor::EditorPage::new),
                        }
                    })
                },
            )
            .expect("open gallery window");
            if !automation::background() {
                cx.activate(true);
            }
        });
}
