// The control's structure, a focusable trigger showing the chosen label and a caret,
// whose menu takes focus on open, puts the highlight back on the committed choice,
// and hands focus back to the trigger on Escape, follows gpui-component 0.7.3
// `select.rs` (Copyright 2024 - 2026 Longbridge, Apache-2.0). Modified: rewritten
// for Keystone's theme over `PopoverMenu` and `VirtualList`, with a plain list of
// labels instead of a searchable delegate, and type-to-jump added.

use crate::color::Color;
use crate::icon::{Icon, IconName};
use crate::label::Label;
use crate::popover::{MenuItem, PopoverMenu};
use crate::styled_ext::h_flex;
use crate::switch::keyed_focus;
use crate::theme::ActiveTheme;
use crate::tokens::{TextSize, radius};
use crate::virtual_list::{VirtualList, VirtualListState};
use gpui::{
    App, Entity, EntityId, FocusHandle, Focusable, IntoElement, KeyBinding, Pixels, RenderOnce,
    SharedString, Window, div, prelude::*, px,
};
use std::rc::Rc;

gpui::actions!(select, [LeaveMenu]);

const MENU_CONTEXT: &str = "SelectMenu";

/// Tab and Shift+Tab bound in the open menu, so they reach it before a window's own
/// Tab binding, which matches higher up the tree.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("tab", LeaveMenu, Some(MENU_CONTEXT)),
        KeyBinding::new("shift-tab", LeaveMenu, Some(MENU_CONTEXT)),
    ]);
}

/// The most rows the open menu shows before it scrolls.
const VISIBLE_ROWS: usize = 8;

type ChangeHandler = Rc<dyn Fn(&usize, &mut Window, &mut App) + 'static>;

/// What a select keeps between frames: whether its menu is open, and the menu's rows.
struct SelectState {
    open: bool,
    list: Entity<VirtualListState>,
}

/// A trigger that shows the chosen option and opens a menu of every option. The caller
/// owns `selected` and stores the index `on_change` reports. Enter, Space, Up, or Down
/// on the focused trigger opens the menu with the choice highlighted. In the menu, the
/// list keys move the highlight, Enter or a click chooses, typing a letter jumps to the
/// next option that starts with it, and Escape closes and refocuses the trigger. Tab
/// and Shift+Tab also close and refocus the trigger, then go on to the window's own
/// binding or key handler, so focus moves on from the trigger. Named `{id}` for the
/// trigger and `{id}-option-{n}` for each option.
#[derive(IntoElement)]
pub struct Select {
    id: SharedString,
    options: Rc<[SharedString]>,
    selected: Option<usize>,
    placeholder: SharedString,
    disabled: bool,
    width: Pixels,
    focus: Option<FocusHandle>,
    on_change: Option<ChangeHandler>,
}

impl Select {
    pub fn new(
        id: impl Into<SharedString>,
        options: impl IntoIterator<Item = impl Into<SharedString>>,
        selected: Option<usize>,
    ) -> Self {
        Self {
            id: id.into(),
            options: options.into_iter().map(Into::into).collect(),
            selected,
            placeholder: "Choose…".into(),
            disabled: false,
            width: px(220.),
            focus: None,
            on_change: None,
        }
    }

    /// The trigger's text while nothing is chosen.
    pub fn placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// The trigger's width, 220 px unless set.
    pub fn width(mut self, width: Pixels) -> Self {
        self.width = width;
        self
    }

    /// Focuses the trigger through `focus` instead of a handle of its own, so a container
    /// can move focus to it.
    pub fn track_focus(mut self, focus: &FocusHandle) -> Self {
        self.focus = Some(focus.clone());
        self
    }

    pub fn on_change(mut self, handler: impl Fn(&usize, &mut Window, &mut App) + 'static) -> Self {
        self.on_change = Some(Rc::new(handler));
        self
    }
}

/// Shows the menu with `selected` highlighted and gives it focus.
fn open(
    state: &Entity<SelectState>,
    count: usize,
    selected: Option<usize>,
    window: &mut Window,
    cx: &mut App,
) {
    let list = state.update(cx, |state, cx| {
        state.open = true;
        cx.notify();
        state.list.clone()
    });
    list.update(cx, |list, cx| {
        list.set_count(count);
        list.move_to(selected.unwrap_or(0));
        cx.notify();
    });
    window.focus(&list.focus_handle(cx), cx);
}

fn close(state: &Entity<SelectState>, cx: &mut App) {
    state.update(cx, |state, cx| {
        state.open = false;
        cx.notify();
    });
}

/// The first option after the highlight, wrapping around, whose label starts with `c`.
fn jump_target(options: &[SharedString], from: Option<usize>, c: char) -> Option<usize> {
    let start = from.map_or(0, |ix| ix + 1);
    (start..options.len())
        .chain(0..start.min(options.len()))
        .find(|&ix| {
            options[ix]
                .chars()
                .next()
                .is_some_and(|first| first.to_lowercase().eq(c.to_lowercase()))
        })
}

impl RenderOnce for Select {
    fn render(mut self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let focus = match self.focus.take() {
            Some(focus) => focus,
            None => keyed_focus(&self.id, window, cx),
        };
        let state = window.use_keyed_state(
            (gpui::ElementId::from(self.id.clone()), "menu"),
            cx,
            |_, cx| SelectState {
                open: false,
                list: cx.new(|cx| VirtualListState::new(0, cx)),
            },
        );
        let host = window.current_view();
        let is_open = state.read(cx).open && !self.disabled;
        let theme = cx.theme();
        let colors = theme.colors;
        let row = theme.density.control(px(28.));
        let (count, selected) = (self.options.len(), self.selected);
        let label = selected
            .and_then(|ix| self.options.get(ix).cloned())
            .map_or((self.placeholder.clone(), Color::Muted), |label| {
                (label, Color::Default)
            });
        let (rest, hover) = if is_open {
            (colors.selection_fill, colors.selection_fill)
        } else {
            (colors.element_hover, colors.element_active)
        };
        let ring = colors.border_focused;
        let trigger = h_flex()
            .id(self.id.clone())
            .w(self.width)
            .h(theme.density.control(px(28.)))
            .px(px(8.))
            .gap_2()
            .rounded(radius::CONTROL)
            .border_1()
            .border_color(colors.border_strong)
            .bg(rest)
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(label.0)
                        .size(TextSize::Base)
                        .color(label.1)
                        .truncate(),
                ),
            )
            .child(
                Icon::new(IconName::Chevron)
                    .size(px(10.))
                    .color(Color::Muted),
            );
        let trigger = crate::automation::tag(trigger, self.id.to_string());
        let Some(on_change) = self.on_change.clone().filter(|_| !self.disabled) else {
            return trigger
                .when(self.disabled, |d| d.opacity(0.4).cursor_not_allowed())
                .into_any_element();
        };
        let trigger = trigger
            .track_focus(&focus)
            .focus(move |s| s.border_color(ring))
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .on_click({
                let state = state.clone();
                move |event, window, cx| {
                    // A pointer press already opened the menu on mouse down.
                    if event.is_keyboard() {
                        open(&state, count, selected, window, cx);
                    }
                }
            })
            .on_key_down({
                let state = state.clone();
                move |event, window, cx| {
                    if matches!(event.keystroke.key.as_str(), "up" | "down")
                        && !event.keystroke.modifiers.modified()
                    {
                        cx.stop_propagation();
                        open(&state, count, selected, window, cx);
                    }
                }
            });
        let list = state.read(cx).list.clone();
        // The caller may change the options while the menu is open.
        if is_open && list.read(cx).count() != count {
            list.update(cx, |list, _| list.set_count(count));
        }
        let menu = is_open.then(|| self.menu(&state, &list, &focus, host, row, on_change));
        PopoverMenu::new(SharedString::from(format!("{}-menu", self.id)), trigger)
            .open(is_open)
            .on_open_change({
                let state = state.clone();
                move |&now_open, window, cx| {
                    if now_open {
                        open(&state, count, selected, window, cx);
                    } else {
                        close(&state, cx);
                    }
                }
            })
            .children(menu)
            .into_any_element()
    }
}

impl Select {
    fn menu(
        &self,
        state: &Entity<SelectState>,
        rows: &Entity<VirtualListState>,
        trigger: &FocusHandle,
        host: EntityId,
        row: gpui::Pixels,
        on_change: ChangeHandler,
    ) -> impl IntoElement + use<> {
        let (id, options, selected) = (self.id.clone(), self.options.clone(), self.selected);
        let list = VirtualList::new(SharedString::from(format!("{id}-options")), rows, {
            let options = options.clone();
            move |ix, highlighted, _, _| {
                let id = id.clone();
                MenuItem::new(ix, options[ix].clone())
                    .checked(selected == Some(ix))
                    .highlighted(highlighted)
                    .debug_selector(move || format!("{id}-option-{ix}"))
                    .into_any_element()
            }
        })
        .on_confirm({
            let (state, trigger) = (state.clone(), trigger.clone());
            move |&ix, window, cx| {
                close(&state, cx);
                window.focus(&trigger, cx);
                on_change(&ix, window, cx);
            }
        })
        .w_full()
        .h(row * options.len().min(VISIBLE_ROWS) as f32);
        let leave = {
            let (state, trigger) = (state.clone(), trigger.clone());
            move |_: &LeaveMenu, window: &mut Window, cx: &mut App| {
                close(&state, cx);
                window.focus(&trigger, cx);
                cx.propagate();
            }
        };
        let (state, trigger, rows) = (state.clone(), trigger.clone(), rows.clone());
        div()
            .key_context(MENU_CONTEXT)
            .on_action(leave)
            .on_key_down(move |event, window, cx| {
                let keystroke = &event.keystroke;
                if keystroke.key == "escape" {
                    cx.stop_propagation();
                    close(&state, cx);
                    window.focus(&trigger, cx);
                    return;
                }
                let mut chars = keystroke.key.chars();
                let (Some(c), None) = (chars.next(), chars.next()) else {
                    return;
                };
                if !c.is_alphanumeric() || keystroke.modifiers.modified() {
                    return;
                }
                cx.stop_propagation();
                let from = rows.read(cx).highlighted();
                if let Some(target) = jump_target(&options, from, c) {
                    rows.update(cx, |rows, cx| {
                        rows.move_to(target);
                        cx.notify();
                    });
                    cx.notify(host);
                }
            })
            .child(list)
    }
}
