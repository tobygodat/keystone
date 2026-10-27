use crate::button::IconButton;
use crate::color::Color;
use crate::fuzzy;
use crate::icon::{Icon, IconName};
use crate::kbd::Kbd;
use crate::label::Label;
use crate::monogram::Monogram;
use crate::styled_ext::{StyledExt, h_flex, v_flex};
use crate::text_input::{self, InputMode, TextInput};
use crate::theme::ActiveTheme;
use crate::tokens::{Elevation, TextSize, radius, spacing};
use crate::tooltip::Tooltip;
use crate::virtual_list::{self, VirtualList, VirtualListState};
use gpui::{
    AnchoredPositionMode, App, Context, Entity, EntityId, FocusHandle, Focusable, FontWeight,
    HighlightStyle, IntoElement, KeyBinding, MouseButton, RenderOnce, SharedString, StyledText,
    Subscription, Window, anchored, deferred, div, point, prelude::*, px,
};
use std::cmp::Reverse;
use std::rc::Rc;

gpui::actions!(command_palette, [Dismiss, PageUp, PageDown, Remove]);

const CONTEXT: &str = "CommandPalette";
const VISIBLE_ROWS: usize = 8;

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", Dismiss, Some(CONTEXT)),
        KeyBinding::new("pageup", PageUp, Some(CONTEXT)),
        KeyBinding::new("pagedown", PageDown, Some(CONTEXT)),
        KeyBinding::new("shift-delete", Remove, Some(CONTEXT)),
    ]);
}

/// One command a palette offers. `id` is what the host acts on; `label` is what the
/// query matches.
#[derive(Clone, Debug)]
pub struct PaletteItem {
    pub id: SharedString,
    pub label: SharedString,
    pub detail: Option<SharedString>,
    pub kbd: Option<SharedString>,
    /// Whether the palette offers to remove it, through `on_remove`.
    pub removable: bool,
    pub monogram: Option<Monogram>,
}

impl PaletteItem {
    pub fn new(id: impl Into<SharedString>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            detail: None,
            kbd: None,
            removable: false,
            monogram: None,
        }
    }

    /// A tile before the label.
    pub fn monogram(mut self, monogram: Monogram) -> Self {
        self.monogram = Some(monogram);
        self
    }

    /// A muted note after the label.
    pub fn detail(mut self, detail: impl Into<SharedString>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// A shortcut hint at the row's end, such as `Ctrl+N`.
    pub fn kbd(mut self, kbd: impl Into<SharedString>) -> Self {
        self.kbd = Some(kbd.into());
        self
    }

    /// Offers to remove the item: a button on its highlighted row, and Shift+Delete.
    pub fn removable(mut self) -> Self {
        self.removable = true;
        self
    }
}

type QueryItem = Box<dyn Fn(&str) -> Option<PaletteItem> + 'static>;

struct Shown {
    item: usize,
    /// The row the query itself made, in place of `item`.
    typed: Option<PaletteItem>,
    /// Byte offsets of the label characters the query matched.
    positions: Vec<usize>,
}

/// The query, the items, and which items the query keeps, best first. Create it
/// when the palette opens and drop it when it closes.
pub struct CommandPaletteState {
    input: Entity<TextInput>,
    list: Entity<VirtualListState>,
    items: Vec<PaletteItem>,
    shown: Vec<Shown>,
    /// Builds a row from the query itself, shown first whenever it gives one.
    query_item: Option<QueryItem>,
    _changed: Subscription,
}

impl CommandPaletteState {
    pub fn new(items: Vec<PaletteItem>, cx: &mut Context<Self>) -> Self {
        Self::with_placeholder(items, "Type a command", cx)
    }

    pub fn with_placeholder(
        items: Vec<PaletteItem>,
        placeholder: &str,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| TextInput::new(placeholder, InputMode::SingleLine, cx));
        let list = cx.new(|cx| VirtualListState::new(items.len(), cx));
        let changed = cx.subscribe(&input, |this, _, _: &text_input::Changed, cx| {
            this.rescore(cx)
        });
        let shown = Self::unfiltered(&items);
        Self {
            input,
            list,
            items,
            shown,
            query_item: None,
            _changed: changed,
        }
    }

    /// Offers a row made from the query, such as a typed path, above every match.
    pub fn set_query_item(
        &mut self,
        item: impl Fn(&str) -> Option<PaletteItem> + 'static,
        cx: &mut Context<Self>,
    ) {
        self.query_item = Some(Box::new(item));
        self.rescore(cx);
    }

    /// Drops the item `id` names, keeping the query and the highlight's place.
    pub fn remove_item(&mut self, id: &str, cx: &mut Context<Self>) {
        let row = self.list.read(cx).highlighted();
        self.items.retain(|item| item.id.as_ref() != id);
        self.rescore(cx);
        if let Some(row) = row {
            let last = self.shown.len().saturating_sub(1);
            self.list.update(cx, |list, _| list.move_to(row.min(last)));
        }
    }

    pub fn input(&self) -> &Entity<TextInput> {
        &self.input
    }

    pub fn query<'a>(&self, cx: &'a App) -> &'a str {
        self.input.read(cx).value()
    }

    /// Replaces the query and filters as if it had been typed.
    pub fn set_query(&mut self, query: &str, cx: &mut Context<Self>) {
        self.input
            .update(cx, |input, cx| input.set_value(query.to_owned(), cx));
        self.rescore(cx);
    }

    /// The items the query keeps, in display order.
    pub fn matches(&self) -> impl Iterator<Item = &PaletteItem> {
        self.shown.iter().map(|shown| self.item(shown))
    }

    pub fn highlighted_item(&self, cx: &App) -> Option<&PaletteItem> {
        let row = self.list.read(cx).highlighted()?;
        self.shown.get(row).map(|shown| self.item(shown))
    }

    fn item<'a>(&'a self, shown: &'a Shown) -> &'a PaletteItem {
        match &shown.typed {
            Some(typed) => typed,
            None => &self.items[shown.item],
        }
    }

    fn unfiltered(items: &[PaletteItem]) -> Vec<Shown> {
        (0..items.len())
            .map(|item| Shown {
                item,
                typed: None,
                positions: Vec::new(),
            })
            .collect()
    }

    /// An empty query shows every item in its original order. Otherwise the best
    /// score comes first, then the shorter label, then the original order.
    fn rescore(&mut self, cx: &mut Context<Self>) {
        let query = self.input.read(cx).value();
        self.shown = if query.is_empty() {
            Self::unfiltered(&self.items)
        } else {
            let mut scored: Vec<(i32, Shown)> = self
                .items
                .iter()
                .enumerate()
                .filter_map(|(item, it)| {
                    let found = fuzzy::score(query, &it.label)?;
                    let positions = found.positions;
                    Some((
                        found.score,
                        Shown {
                            item,
                            typed: None,
                            positions,
                        },
                    ))
                })
                .collect();
            scored.sort_by_key(|(score, shown)| {
                (Reverse(*score), self.items[shown.item].label.len())
            });
            scored.into_iter().map(|(_, shown)| shown).collect()
        };
        if let Some(typed) = self.query_item.as_ref().and_then(|make| make(query)) {
            self.shown.insert(
                0,
                Shown {
                    item: 0,
                    typed: Some(typed),
                    positions: Vec::new(),
                },
            );
        }
        let count = self.shown.len();
        self.list.update(cx, |list, cx| {
            list.set_count(count);
            list.move_to(0);
            cx.notify();
        });
        cx.notify();
    }
}

impl Focusable for CommandPaletteState {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

type ConfirmHandler = Rc<dyn Fn(&PaletteItem, &mut Window, &mut App) + 'static>;
type RemoveHandler = Rc<dyn Fn(&PaletteItem, &mut Window, &mut App) + 'static>;
type DismissHandler = Rc<dyn Fn(&mut Window, &mut App) + 'static>;

/// A centered overlay that filters commands as the user types. The input keeps focus
/// throughout: Up and Down move the highlight, PageUp and PageDown move a page,
/// Enter or a click calls `on_confirm`, and Escape or a press outside the card calls
/// `on_dismiss`. The host owns whether it is open and focuses
/// [`CommandPaletteState`] when it opens.
///
/// With `on_remove`, a highlighted removable row shows a remove button, and
/// Shift+Delete removes it too.
///
/// The card is `{id}`, the input `{id}-input`, the rows `{id}-row-{n}`, and a row's
/// remove button `{id}-remove`, for tests and native verification.
#[derive(IntoElement)]
pub struct CommandPalette {
    id: SharedString,
    state: Entity<CommandPaletteState>,
    on_confirm: Option<ConfirmHandler>,
    on_dismiss: Option<DismissHandler>,
    on_remove: Option<RemoveHandler>,
    empty: SharedString,
}

impl CommandPalette {
    pub fn new(id: impl Into<SharedString>, state: &Entity<CommandPaletteState>) -> Self {
        Self {
            id: id.into(),
            state: state.clone(),
            on_confirm: None,
            on_dismiss: None,
            on_remove: None,
            empty: "No matching commands".into(),
        }
    }

    pub fn on_remove(
        mut self,
        handler: impl Fn(&PaletteItem, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_remove = Some(Rc::new(handler));
        self
    }

    /// What the card says when the query keeps nothing.
    pub fn empty_label(mut self, label: impl Into<SharedString>) -> Self {
        self.empty = label.into();
        self
    }

    pub fn on_confirm(
        mut self,
        handler: impl Fn(&PaletteItem, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_confirm = Some(Rc::new(handler));
        self
    }

    pub fn on_dismiss(mut self, handler: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_dismiss = Some(Rc::new(handler));
        self
    }
}

fn render_row(
    id: &SharedString,
    state: &Entity<CommandPaletteState>,
    row: usize,
    highlighted: bool,
    height: gpui::Pixels,
    remove: Option<&Rc<dyn Fn(usize, &mut Window, &mut App)>>,
    cx: &mut App,
) -> gpui::AnyElement {
    let state = state.read(cx);
    let Some(shown) = state.shown.get(row) else {
        return div().h(height).into_any_element();
    };
    let item = state.item(shown);
    let marked = HighlightStyle {
        color: Some(Color::Accent.to_hsla(cx)),
        font_weight: Some(FontWeight::SEMIBOLD),
        ..Default::default()
    };
    let label = StyledText::new(item.label.clone()).with_highlights(
        crate::fuzzy::ranges(&item.label, &shown.positions)
            .into_iter()
            .map(|range| (range, marked)),
    );
    h_flex()
        .h(height)
        .px(spacing::SM)
        .gap(spacing::SM)
        .rounded(radius::MENU_ROW)
        .when(highlighted, |d| d.bg(cx.theme().colors.selection_fill))
        .children(item.monogram.clone())
        .child(div().flex_none().child(label))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .children(item.detail.clone().map(|detail| {
                    Label::new(detail)
                        .size(TextSize::Sm)
                        .color(Color::Muted)
                        .truncate()
                })),
        )
        .children(item.kbd.clone().map(Kbd::new))
        .children(
            remove
                .filter(|_| highlighted && item.removable)
                .map(|remove| {
                    let remove = remove.clone();
                    IconButton::new(SharedString::from(format!("{id}-remove")), IconName::Close)
                        .debug_selector({
                            let id = id.clone();
                            move || format!("{id}-remove")
                        })
                        .tooltip(Tooltip::text("Remove from list · Shift+Delete"))
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            remove(row, window, cx)
                        })
                }),
        )
        .into_any_element()
}

fn move_highlight(
    list: &Entity<VirtualListState>,
    host: EntityId,
    cx: &mut App,
    edit: impl FnOnce(&mut VirtualListState),
) {
    virtual_list::change(list, host, cx, |list| {
        edit(list);
        true
    });
}

impl RenderOnce for CommandPalette {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let host = window.current_view();
        let id = self.id;
        let state = self.state.read(cx);
        let (input, list, count) = (state.input.clone(), state.list.clone(), state.shown.len());
        let row_height = cx.theme().density.control(px(32.));
        let dismiss = {
            let on_dismiss = self.on_dismiss;
            move |window: &mut Window, cx: &mut App| {
                if let Some(on_dismiss) = &on_dismiss {
                    on_dismiss(window, cx)
                }
            }
        };
        let remove = self.on_remove.map(|on_remove| {
            let state = self.state.clone();
            Rc::new(move |row: usize, window: &mut Window, cx: &mut App| {
                let state = state.read(cx);
                let item = state
                    .shown
                    .get(row)
                    .map(|shown| state.item(shown).clone())
                    .filter(|item| item.removable);
                if let Some(item) = item {
                    on_remove(&item, window, cx);
                }
            }) as Rc<dyn Fn(usize, &mut Window, &mut App)>
        });
        let confirm = {
            let (state, on_confirm) = (self.state.clone(), self.on_confirm);
            Rc::new(move |row: usize, window: &mut Window, cx: &mut App| {
                let state = state.read(cx);
                let item = state.shown.get(row).map(|shown| state.item(shown).clone());
                if let (Some(item), Some(on_confirm)) = (item, &on_confirm) {
                    on_confirm(&item, window, cx);
                }
            })
        };
        let field = div()
            .flex_1()
            .min_w_0()
            .capture_action({
                let list = list.clone();
                move |_: &text_input::Up, _, cx| {
                    move_highlight(&list, host, cx, |list| list.move_by(-1));
                    cx.stop_propagation();
                }
            })
            .capture_action({
                let list = list.clone();
                move |_: &text_input::Down, _, cx| {
                    move_highlight(&list, host, cx, |list| list.move_by(1));
                    cx.stop_propagation();
                }
            })
            .capture_action({
                let (list, confirm) = (list.clone(), confirm.clone());
                move |_: &text_input::Newline, window, cx| {
                    cx.stop_propagation();
                    if let Some(row) = list.read(cx).highlighted() {
                        confirm(row, window, cx);
                    }
                }
            })
            .child(input);
        let field = crate::automation::tag(field, format!("{id}-input"));
        let input_row = h_flex()
            .px(spacing::MD)
            .py(spacing::XS)
            .gap(spacing::SM)
            .hairline_b(cx)
            .child(
                Icon::new(IconName::Search)
                    .size(px(16.))
                    .color(Color::Muted),
            )
            .child(field);
        let results = if count == 0 {
            div()
                .px(spacing::MD)
                .py(spacing::MD)
                .child(Label::new(self.empty.clone()).color(Color::Muted))
                .into_any_element()
        } else {
            let rows = {
                let (id, state, remove) = (id.clone(), self.state.clone(), remove.clone());
                move |row, highlighted, _: &mut Window, cx: &mut App| {
                    render_row(
                        &id,
                        &state,
                        row,
                        highlighted,
                        row_height,
                        remove.as_ref(),
                        cx,
                    )
                }
            };
            div()
                .p(spacing::XS)
                .child(
                    VirtualList::new(id.clone(), &list, rows)
                        .focusable(false)
                        .on_confirm(move |row, window, cx| confirm(*row, window, cx))
                        .h(row_height * count.min(VISIBLE_ROWS) as f32),
                )
                .into_any_element()
        };
        let card = v_flex()
            .id(id.clone())
            .key_context(CONTEXT)
            .occlude()
            .w(px(560.))
            .max_w_full()
            .surface(Elevation::Overlay, cx)
            .on_action({
                let dismiss = dismiss.clone();
                move |_: &Dismiss, window, cx| dismiss(window, cx)
            })
            .on_action({
                let list = list.clone();
                move |_: &PageUp, _, cx| move_highlight(&list, host, cx, |list| list.page(-1))
            })
            .on_action({
                let list = list.clone();
                move |_: &PageDown, _, cx| move_highlight(&list, host, cx, |list| list.page(1))
            })
            .on_action({
                let list = list.clone();
                move |_: &Remove, window, cx| {
                    if let (Some(row), Some(remove)) = (list.read(cx).highlighted(), &remove) {
                        remove(row, window, cx);
                    }
                }
            })
            .child(input_row)
            .child(results);
        let card = crate::automation::tag(card, id.to_string());
        let viewport = window.viewport_size();
        let scrim = div()
            .id(SharedString::from(format!("{id}-scrim")))
            .w(viewport.width)
            .h(viewport.height)
            .occlude()
            .bg(cx.theme().colors.scrim)
            .flex()
            .flex_col()
            .items_center()
            .pt(viewport.height * 0.12)
            .px(spacing::XL)
            .on_mouse_down(MouseButton::Left, move |_, window, cx| dismiss(window, cx))
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
