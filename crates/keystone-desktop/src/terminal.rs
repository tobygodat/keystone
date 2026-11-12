//! The integrated terminal: shells for a session, in a drawer below the window's main
//! area. The shells belong to the app, not to a window, so closing a window keeps them,
//! their output and their scrollback, and any window showing the session shows them.
//! Quitting ends them. See `docs/design/terminal.md`.

mod element;
mod input;
mod pane;

use crate::Keystone;
use crate::presentation::SessionPlace;
use alacritty_terminal::event::Event;
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::{Column, Line, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::Rgb;
use futures::StreamExt;
use futures::channel::mpsc::UnboundedSender;
use gpui::{
    AnyElement, App, AppContext, Axis, ClipboardItem, Context, CursorStyle, Empty, Entity, Global,
    KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, NoAction, Pixels,
    Render, ScrollDelta, ScrollWheelEvent, StyleRefinement, Task, Window, WindowId, div,
    prelude::*, px,
};
use keystone_ui::prelude::*;
use pane::{GRACE, Pane, Shell, Stopping};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

gpui::actions!(
    terminal,
    [
        ToggleTerminal,
        SplitTerminal,
        CloseTerminal,
        FocusNextPane,
        FocusPreviousPane,
        CopySelection,
        Paste
    ]
);

/// T3 Code's limit on terminals side by side.
pub(crate) const MAX_PANES: usize = 4;
const MIN_HEIGHT: Pixels = px(120.);
const DEFAULT_HEIGHT: Pixels = px(300.);
pub(crate) const CONTEXT: &str = "Terminal";

/// Every session's terminal, by session id.
pub(crate) struct Terminals {
    sessions: HashMap<u64, Entity<SessionTerminal>>,
    shell: Shell,
    /// Shells of closed panes that may not have gone yet, so Quit waits for them too.
    stopping: Vec<Stopping>,
}

impl Global for Terminals {}

pub(crate) fn install(cx: &mut App) {
    cx.set_global(Terminals {
        sessions: HashMap::new(),
        shell: Shell::login(),
        stopping: Vec::new(),
    });
    cx.on_window_closed(|cx, _| cx.defer(refit)).detach();
}

/// Runs `program` instead of the login shell in panes started from now on.
#[cfg(test)]
pub(crate) fn use_shell(program: &str, args: &[&str], cx: &mut App) {
    cx.global_mut::<Terminals>().shell = Shell {
        program: program.into(),
        args: args.iter().map(|arg| (*arg).into()).collect(),
        env: Vec::new(),
    };
}

pub(crate) fn session_terminal(session: u64, cx: &App) -> Option<Entity<SessionTerminal>> {
    cx.try_global::<Terminals>()?
        .sessions
        .get(&session)
        .cloned()
}

/// The windows showing `session`, whose terminal is open there when it has one.
pub(crate) fn showing(session: u64, cx: &App) -> Vec<WindowId> {
    crate::open_windows(cx)
        .into_iter()
        .filter_map(|view| {
            let view = view.read(cx);
            (view.settings.is_none() && view.selected(cx) == Some(session))
                .then(|| view.handle.window_id())
        })
        .collect()
}

/// Fits every pane to the windows that show it and are on screen, so a window that
/// switched session, opened Settings, closed or was hidden gives up its claim on the
/// PTY's size. A drawer is a cached view and repaints only when told, so a pane whose
/// size changed repaints wherever it shows.
pub(crate) fn refit(cx: &mut App) {
    let Some(terminals) = cx.try_global::<Terminals>() else {
        return;
    };
    let sessions: Vec<_> = terminals
        .sessions
        .iter()
        .map(|(session, terminal)| (*session, terminal.clone()))
        .collect();
    for (session, terminal) in sessions {
        let on_screen: Vec<WindowId> = showing(session, cx)
            .into_iter()
            .filter(|id| {
                cx.windows()
                    .into_iter()
                    .find(|handle| handle.window_id() == *id)
                    .and_then(|handle| handle.update(cx, |_, window, _| presented(window)).ok())
                    .unwrap_or(false)
            })
            .collect();
        terminal.update(cx, |terminal, cx| {
            let mut changed = false;
            for pane in &terminal.panes {
                changed |= pane.fit(|id| on_screen.contains(&id));
            }
            if changed {
                cx.notify();
            }
        });
    }
}

/// Whether `window`'s frames are seen, so its drawer's size counts: it is on screen,
/// or it is a background launch's, whose covered windows the automation inbox draws
/// and captures.
fn presented(window: &Window) -> bool {
    window.is_visible() || keystone_ui::automation::background()
}

/// Ends every shell, closed panes' shells still going included, waiting at most about
/// [`GRACE`] for them to go before killing the rest.
pub(crate) fn shutdown_all(cx: &mut App) {
    if !cx.has_global::<Terminals>() {
        return;
    }
    let terminals = cx.global_mut::<Terminals>();
    let sessions: Vec<_> = terminals.sessions.drain().map(|(_, t)| t).collect();
    let mut stopping = std::mem::take(&mut terminals.stopping);
    for terminal in sessions {
        stopping.extend(terminal.update(cx, |terminal, _| {
            terminal.open = false;
            terminal.panes.drain(..).map(Pane::stop).collect::<Vec<_>>()
        }));
    }
    let deadline = Instant::now() + GRACE;
    for shell in stopping {
        shell.finish_by(deadline);
    }
}

pub(crate) fn bind_keys(cx: &mut App) {
    // Bound after the app's own keys: these win over app bindings of equal depth.
    let mut bindings = shell_keys(crate::keymap::current(cx), !cfg!(target_os = "macos"));
    if cfg!(target_os = "macos") {
        bindings.push(KeyBinding::new("secondary-c", CopySelection, Some(CONTEXT)));
        bindings.push(KeyBinding::new("secondary-v", Paste, Some(CONTEXT)));
    } else {
        bindings.push(KeyBinding::new(
            "secondary-shift-c",
            CopySelection,
            Some(CONTEXT),
        ));
        bindings.push(KeyBinding::new("secondary-shift-v", Paste, Some(CONTEXT)));
    }
    cx.bind_keys(bindings);
}

/// The keys that go to the shell rather than the app inside a pane, as `NoAction`
/// bindings. With `control_keys`, as on Linux where the app's shortcuts are Ctrl, that
/// is every Ctrl+letter too. The commands that work from inside the terminal keep
/// their keys, as the user bound them.
pub(crate) fn shell_keys(keymap: &crate::keymap::Keymap, control_keys: bool) -> Vec<KeyBinding> {
    let mut shell: Vec<String> = ["tab", "shift-tab", "secondary-enter", "f2"]
        .map(String::from)
        .into();
    if control_keys {
        shell.extend(
            ('a'..='z')
                .map(String::from)
                .chain(["[", "]", "\\", "space"].map(String::from))
                .map(|key| format!("secondary-{key}")),
        );
    }
    let kept: Vec<&str> = crate::commands::COMMANDS
        .iter()
        .filter(|command| command.in_terminal)
        .map(|command| keymap.keys(command))
        .collect();
    shell
        .iter()
        .filter(|keys| !kept.contains(&keys.as_str()))
        .map(|keys| KeyBinding::new(keys, NoAction, Some(CONTEXT)))
        .collect()
}

/// A session's shells and how its drawer looks.
pub(crate) struct SessionTerminal {
    session: u64,
    directory: PathBuf,
    panes: Vec<Pane>,
    focused: usize,
    open: bool,
    height: Pixels,
    split: Entity<SplitState>,
    next_pane: u64,
    /// The pane a left press started selecting in.
    selecting: Option<u64>,
    error: Option<String>,
    events: UnboundedSender<(u64, Event)>,
    _pump: Task<()>,
}

/// The drawer's top edge, while it is dragged to resize the drawer.
#[derive(Clone, Copy)]
struct ResizeDrawer;

struct ResizeDrawerPreview;

impl Render for ResizeDrawerPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

impl SessionTerminal {
    fn start(session: u64, directory: PathBuf, cx: &mut App) -> std::io::Result<Entity<Self>> {
        let terminal = cx.new(|cx| {
            let (events, mut received) = futures::channel::mpsc::unbounded();
            let pump = cx.spawn(async move |this: gpui::WeakEntity<Self>, cx| {
                while let Some(first) = received.next().await {
                    let mut batch = vec![first];
                    while let Ok(next) = received.try_recv() {
                        batch.push(next);
                    }
                    if this.update(cx, |this, cx| this.apply(batch, cx)).is_err() {
                        break;
                    }
                }
            });
            Self {
                session,
                directory,
                panes: Vec::new(),
                focused: 0,
                open: true,
                height: DEFAULT_HEIGHT,
                split: cx.new(|_| SplitState::new(Axis::Horizontal, 1).min_px(px(160.))),
                next_pane: 1,
                selecting: None,
                error: None,
                events,
                _pump: pump,
            }
        });
        terminal.update(cx, |terminal, cx| terminal.add_pane(cx))?;
        cx.global_mut::<Terminals>()
            .sessions
            .insert(session, terminal.clone());
        Ok(terminal)
    }

    fn add_pane(&mut self, cx: &mut Context<Self>) -> std::io::Result<()> {
        let shell = cx.global::<Terminals>().shell.clone();
        let pane = Pane::spawn(
            self.next_pane,
            &shell,
            &self.directory,
            self.events.clone(),
            cx.focus_handle(),
        )?;
        self.next_pane += 1;
        self.panes.push(pane);
        self.focused = self.panes.len() - 1;
        let count = self.panes.len();
        let mut shares = self.split.read(cx).fractions().to_vec();
        if count > 1 {
            // The new pane takes 1/count and the others keep their proportions.
            shares.truncate(count - 1);
            shares.push(shares.iter().sum::<f32>() / (count - 1) as f32);
        }
        self.relayout(&shares, cx);
        Ok(())
    }

    /// Lays the panes out at `shares`, scaled to fill the drawer.
    fn relayout(&mut self, shares: &[f32], cx: &mut Context<Self>) {
        self.split.update(cx, |split, cx| {
            split.set_fractions(shares);
            cx.notify();
        });
        cx.notify();
    }

    pub(crate) fn pane(&self, id: u64) -> Option<&Pane> {
        self.panes.iter().find(|pane| pane.id == id)
    }

    fn contains_focus(&self, window: &Window) -> bool {
        self.panes.iter().any(|pane| pane.focus.is_focused(window))
    }

    /// The pane focused in `window`, else the one focused last in any window.
    fn current(&self, window: &Window) -> usize {
        self.panes
            .iter()
            .position(|pane| pane.focus.is_focused(window))
            .unwrap_or(self.focused)
            .min(self.panes.len().saturating_sub(1))
    }

    fn focus_current(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_pane(self.current(window), window, cx);
    }

    fn focus_pane(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pane) = self.panes.get(index) {
            self.focused = index;
            pane.focus.focus(window, cx);
        }
        cx.notify();
    }

    /// Focuses the pane `step` places from the current one, wrapping around.
    fn focus_step(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.panes.len() as isize;
        if count == 0 {
            return;
        }
        let index = (self.current(window) as isize + step).rem_euclid(count);
        self.focus_pane(index as usize, window, cx);
    }

    fn split_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.panes.len() >= MAX_PANES {
            return;
        }
        self.error = self
            .add_pane(cx)
            .err()
            .map(|error| format!("Terminal did not start. {error}"));
        self.focus_pane(self.focused, window, cx);
    }

    fn close_focused(&mut self, window: &Window, cx: &mut Context<Self>) {
        if let Some(id) = self.panes.get(self.current(window)).map(|pane| pane.id) {
            self.close_pane(id, cx);
        }
    }

    /// Ends the pane's shell. Focus that was in it moves to the next pane, or to the
    /// composer when it was the last; then the drawer hides and the session's entry
    /// goes, so the next toggle starts a fresh shell.
    fn close_pane(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(index) = self.panes.iter().position(|pane| pane.id == id) else {
            return;
        };
        let pane = self.panes.remove(index);
        let lost = pane.focus.clone();
        let stopping = pane.stop();
        stopping.detach();
        let terminals = cx.global_mut::<Terminals>();
        terminals.stopping.retain(|shell| !shell.is_done());
        terminals.stopping.push(stopping);
        if self.selecting == Some(id) {
            self.selecting = None;
        }
        if self.focused > index {
            self.focused -= 1;
        }
        self.focused = self.focused.min(self.panes.len().saturating_sub(1));
        let next = self.panes.get(self.focused).map(|pane| pane.focus.clone());
        if self.panes.is_empty() {
            self.open = false;
            cx.global_mut::<Terminals>().sessions.remove(&self.session);
        }
        let mut shares = self.split.read(cx).fractions().to_vec();
        if shares.len() > 1 && index < shares.len() {
            shares.remove(index);
        }
        self.relayout(&shares, cx);
        cx.defer(move |cx| {
            for window in cx.windows() {
                let _ = window.update(cx, |_, window, cx| {
                    if !lost.is_focused(window) {
                        return;
                    }
                    match &next {
                        Some(next) => next.focus(window, cx),
                        None => window.dispatch_action(Box::new(crate::FocusComposer), cx),
                    }
                });
            }
        });
    }

    fn apply(&mut self, events: Vec<(u64, Event)>, cx: &mut Context<Self>) {
        let mut changed = false;
        for (id, event) in events {
            let Some(pane) = self.panes.iter_mut().find(|pane| pane.id == id) else {
                continue;
            };
            match event {
                Event::Wakeup => changed = true,
                Event::PtyWrite(text) => pane.write(text.into_bytes()),
                Event::ClipboardStore(_, text) => {
                    cx.write_to_clipboard(ClipboardItem::new_string(text))
                }
                Event::ColorRequest(index, format) => {
                    let color = reported_color(index, cx);
                    pane.write(format(color).into_bytes());
                }
                Event::TextAreaSizeRequest(format) => {
                    pane.write(format(pane.window_size()).into_bytes())
                }
                Event::Exit | Event::ChildExit(_) => {
                    pane.exited = true;
                    self.close_pane(id, cx);
                }
                _ => {}
            }
        }
        if changed {
            cx.notify();
        }
    }

    fn key_down(&mut self, id: u64, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(pane) = self.pane(id) else { return };
        let mode = *pane.term.lock().mode();
        let Some(bytes) = input::bytes(&event.keystroke, mode) else {
            return;
        };
        {
            let mut term = pane.term.lock();
            term.scroll_display(Scroll::Bottom);
            term.selection = None;
        }
        pane.write(bytes);
        cx.stop_propagation();
        cx.notify();
    }

    fn copy(&mut self, id: u64, cx: &mut Context<Self>) {
        let text = self
            .pane(id)
            .and_then(|pane| pane.term.lock().selection_to_string());
        match text {
            Some(text) if !text.is_empty() => {
                cx.write_to_clipboard(ClipboardItem::new_string(text))
            }
            _ => cx.propagate(),
        }
    }

    fn paste(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let Some(pane) = self.pane(id) else { return };
        let mode = *pane.term.lock().mode();
        pane.term.lock().scroll_display(Scroll::Bottom);
        pane.write(input::paste(&text, mode));
        cx.notify();
    }

    fn mouse_down(
        &mut self,
        id: u64,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.panes.iter().position(|pane| pane.id == id) else {
            return;
        };
        self.focused = index;
        let pane = &self.panes[index];
        pane.focus.focus(window, cx);
        if let Some((point, side)) = grid_point(pane, window, event.position) {
            let kind = match event.click_count {
                2 => SelectionType::Semantic,
                3 => SelectionType::Lines,
                _ => SelectionType::Simple,
            };
            pane.term.lock().selection = Some(Selection::new(kind, point, side));
            self.selecting = Some(id);
        }
        cx.notify();
    }

    fn mouse_move(
        &mut self,
        id: u64,
        event: &MouseMoveEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.selecting != Some(id) || event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        let Some(pane) = self.pane(id) else { return };
        if let Some((point, side)) = grid_point(pane, window, event.position)
            && let Some(selection) = pane.term.lock().selection.as_mut()
        {
            selection.update(point, side);
        }
        cx.notify();
    }

    fn wheel(&mut self, id: u64, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let Some(pane) = self.pane(id) else { return };
        let lines = match event.delta {
            ScrollDelta::Lines(delta) => delta.y,
            ScrollDelta::Pixels(delta) => delta.y / pane.size().cell.height,
        } + pane.scroll_remainder.get();
        let whole = lines.trunc();
        pane.scroll_remainder.set(lines - whole);
        if whole == 0. {
            return;
        }
        let mode = *pane.term.lock().mode();
        if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
            let arrow: &[u8] = match (whole > 0., mode.contains(TermMode::APP_CURSOR)) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            pane.write(arrow.repeat(whole.abs() as usize));
        } else {
            pane.scroll(Scroll::Delta(whole as i32));
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let full = self.panes.len() >= MAX_PANES;
        let folder = self.directory.file_name().map_or_else(
            || self.directory.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        let split = IconButton::new("terminal-split", IconName::Split)
            .size(ButtonSize::Compact)
            .debug_selector(|| "terminal-split".into())
            .disabled(full)
            .tooltip(Tooltip::with_key(
                "Split terminal",
                crate::commands::hint("split-terminal", cx),
            ))
            .on_click(cx.listener(|this, _, window, cx| this.split_pane(window, cx)));
        h_flex()
            .flex_shrink_0()
            .h(cx.theme().density.control(px(34.)))
            .pl(px(12.))
            .pr_1()
            .gap_2()
            .child(
                Icon::new(IconName::Terminal)
                    .size(px(16.))
                    .color(Color::Muted),
            )
            .child(
                Label::new("Terminal")
                    .size(TextSize::Ui)
                    .weight(gpui::FontWeight::MEDIUM),
            )
            .child(
                Label::new(folder)
                    .size(TextSize::Ui)
                    .color(Color::Muted)
                    .truncate(),
            )
            .children(self.error.clone().map(|error| {
                Label::new(error)
                    .size(TextSize::Ui)
                    .color(Color::Danger)
                    .truncate()
            }))
            .child(div().flex_1())
            .child(if full {
                div()
                    .id("terminal-split-limit")
                    .tooltip(Tooltip::text("Up to 4 terminals side by side").builder())
                    .child(split)
                    .into_any_element()
            } else {
                split.into_any_element()
            })
            .child(
                IconButton::new("terminal-close", IconName::Close)
                    .size(ButtonSize::Compact)
                    .debug_selector(|| "terminal-close".into())
                    .tooltip(Tooltip::with_key(
                        "Close terminal",
                        crate::commands::hint("close-terminal", cx),
                    ))
                    .on_click(cx.listener(|this, _, window, cx| this.close_focused(window, cx))),
            )
    }

    fn pane_view(&self, index: usize, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let pane = &self.panes[index];
        let id = pane.id;
        let focused = pane.focus.is_focused(window);
        let several = self.panes.len() > 1;
        let group = format!("terminal-pane-{index}");
        div()
            .id(("terminal-pane", id))
            .automation_id(format!("terminal-pane-{index}"))
            .group(group.clone())
            .relative()
            .size_full()
            .p(px(4.))
            .bg(colors.terminal.background)
            .border_1()
            // One pane's filled cursor already shows it has focus; the ring tells split
            // panes apart.
            .border_color(if focused && several {
                colors.border_focused
            } else {
                colors.terminal.background
            })
            .track_focus(&pane.focus)
            .key_context(CONTEXT)
            .cursor(CursorStyle::IBeam)
            .on_key_down(cx.listener(move |this, event, _, cx| this.key_down(id, event, cx)))
            .on_action(cx.listener(move |this, _: &CopySelection, _, cx| this.copy(id, cx)))
            .on_action(cx.listener(move |this, _: &Paste, _, cx| this.paste(id, cx)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event, window, cx| this.mouse_down(id, event, window, cx)),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.selecting = None),
            )
            .on_mouse_move(
                cx.listener(move |this, event, window, cx| this.mouse_move(id, event, window, cx)),
            )
            .on_scroll_wheel(cx.listener(move |this, event, _, cx| this.wheel(id, event, cx)))
            .child(element::Grid {
                terminal: cx.weak_entity(),
                pane: id,
                focused,
            })
            .when(several, |d| {
                d.child(
                    div()
                        .absolute()
                        .top(px(6.))
                        .right(px(6.))
                        .invisible()
                        .group_hover(group, |s| s.visible())
                        .child(
                            IconButton::new(("terminal-pane-close", id), IconName::Close)
                                .size(ButtonSize::Compact)
                                .debug_selector(move || format!("terminal-pane-{index}-close"))
                                .tooltip(Tooltip::text("Close this terminal"))
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.close_pane(id, cx)),
                                ),
                        ),
                )
            })
            .into_any_element()
    }
}

impl Render for SessionTerminal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors;
        let entity = cx.entity();
        let panes: Vec<AnyElement> = (0..self.panes.len())
            .map(|index| self.pane_view(index, window, cx))
            .collect();
        let body = if panes.len() > 1 {
            Split::new("terminal-panes", &self.split)
                .children(panes)
                .into_any_element()
        } else {
            div().size_full().children(panes).into_any_element()
        };
        v_flex()
            .id("terminal-drawer")
            .automation_id("terminal-drawer")
            .relative()
            .size_full()
            .bg(colors.terminal.background)
            .border_t_1()
            .border_color(colors.border)
            // A press on the header keeps focus where it was, so Close knows which
            // pane had it, rather than letting the window root take it.
            .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
            .on_drag_move::<ResizeDrawer>(move |event, window, cx| {
                let bottom = event.bounds.bottom();
                let main = window
                    .root::<Keystone>()
                    .flatten()
                    .map_or(px(0.), |root| root.read(cx).main_min_height());
                let height = clamp_height(bottom - event.event.position.y, main, window, cx);
                entity.update(cx, |terminal, cx| {
                    if terminal.height != height {
                        terminal.height = height;
                        cx.notify();
                    }
                });
            })
            .child(self.header(cx))
            .child(div().flex_1().min_h_0().child(body))
            .child(
                div()
                    .id("terminal-resize")
                    .automation_id("terminal-resize")
                    .absolute()
                    .top(px(-3.))
                    .left_0()
                    .right_0()
                    .h(px(6.))
                    .cursor_row_resize()
                    .on_drag(ResizeDrawer, |_, _, _, cx| {
                        cx.stop_propagation();
                        cx.new(|_| ResizeDrawerPreview)
                    }),
            )
    }
}

/// The drawer's height within what the window has room for: at least 120 px, and at
/// most what leaves the top bar and `main`, the main area's own minimum, which holds
/// the composer at full height and some of the transcript. In a window too short for
/// both, the drawer keeps its minimum and the main area gives, since a terminal of
/// fewer lines is no use and the shortcut hides it.
fn clamp_height(height: Pixels, main: Pixels, window: &Window, cx: &App) -> Pixels {
    let top_bar = cx.theme().density.control(crate::TOP_BAR);
    let most = window.viewport_size().height - top_bar - main;
    height.clamp(MIN_HEIGHT, most.max(MIN_HEIGHT))
}

/// The cell under `position` in `window`, and which half of it, in grid coordinates.
fn grid_point(
    pane: &Pane,
    window: &Window,
    position: gpui::Point<Pixels>,
) -> Option<(GridPoint, Side)> {
    let view = pane.view(window.window_handle().window_id())?;
    let size = pane.size();
    let local = position - view.bounds.origin;
    let column = (local.x / size.cell.width).max(0.);
    let row = ((local.y / size.cell.height).max(0.) as usize).min(size.lines - 1);
    let side = if column.fract() < 0.5 {
        Side::Left
    } else {
        Side::Right
    };
    let column = (column as usize).min(size.columns - 1);
    let offset = pane.term.lock().grid().display_offset();
    Some((
        GridPoint::new(Line(row as i32 - offset as i32), Column(column)),
        side,
    ))
}

/// A color a program asked for (OSC 4, 10, 11 or 12), from the theme.
fn reported_color(index: usize, cx: &App) -> Rgb {
    let palette = cx.theme().colors.terminal;
    let color = match index {
        0..256 => palette.indexed(index as u8),
        256 => palette.foreground,
        257 => palette.background,
        _ => palette.cursor,
    }
    .to_rgb();
    let channel = |value: f32| (value * 255.).round() as u8;
    Rgb {
        r: channel(color.r),
        g: channel(color.g),
        b: channel(color.b),
    }
}

/// The folder a session's shell starts in: its recorded folder, a worktree session's
/// being its worktree, else the user's home. A session on another machine gets no
/// shell here, and `Err` names its machine.
fn start_directory(place: Option<SessionPlace>) -> Result<PathBuf, String> {
    let home = || std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
    match place {
        Some(SessionPlace::Local(directory)) if directory.path().is_dir() => {
            Ok(directory.path().to_path_buf())
        }
        Some(SessionPlace::Local(_) | SessionPlace::Unusable) | None => Ok(home()),
        Some(SessionPlace::Remote(machine)) => Err(machine),
    }
}

impl Keystone {
    /// Opens the selected session's terminal, starting a shell if it has none, and
    /// focuses it. When it is open and focused, hides it and returns to the composer;
    /// open but not focused, focuses it.
    pub(crate) fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected_session(cx).map(|session| session.id()) else {
            self.error = Some("No session open.".into());
            return self.refresh(cx);
        };
        let directory = match start_directory(self.session_place(id, cx)) {
            Ok(directory) => directory,
            Err(_) => return self.toggle_elsewhere(id, window, cx),
        };
        let terminal = match session_terminal(id, cx) {
            Some(terminal) => terminal,
            None => match SessionTerminal::start(id, directory, cx) {
                Ok(terminal) => terminal,
                Err(error) => {
                    self.error = Some(format!("Terminal did not start. {error}"));
                    return self.refresh(cx);
                }
            },
        };
        let read = terminal.read(cx);
        if read.open && read.contains_focus(window) {
            return self.hide_terminal(terminal, window, cx);
        }
        terminal.update(cx, |terminal, cx| {
            terminal.open = true;
            terminal.focus_current(window, cx);
        });
        cx.notify();
    }

    /// Shows or hides the drawer that says `session`'s terminal isn't available here.
    fn toggle_elsewhere(&mut self, session: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.elsewhere_drawer == Some(session) {
            self.elsewhere_drawer = None;
            self.focus_composer(window, cx);
        } else {
            self.elsewhere_drawer = Some(session);
        }
        cx.notify();
    }

    /// The machine the selected session runs on, while this window's drawer says its
    /// terminal isn't available here.
    fn terminal_elsewhere(&self, cx: &App) -> Option<String> {
        if self.settings.is_some() {
            return None;
        }
        let session = self.selected(cx)?;
        if self.elsewhere_drawer != Some(session) {
            return None;
        }
        match self.session_place(session, cx)? {
            SessionPlace::Remote(machine) => Some(machine),
            SessionPlace::Local(_) | SessionPlace::Unusable => None,
        }
    }

    fn hide_terminal(
        &mut self,
        terminal: Entity<SessionTerminal>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        terminal.update(cx, |terminal, cx| {
            terminal.open = false;
            cx.notify();
        });
        self.focus_composer(window, cx);
        cx.notify();
    }

    pub(crate) fn split_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.shown_terminal(cx) {
            Some(terminal) => terminal.update(cx, |terminal, cx| terminal.split_pane(window, cx)),
            None if self.terminal_elsewhere(cx).is_some() => {}
            None => self.toggle_terminal(window, cx),
        }
    }

    pub(crate) fn close_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(terminal) = self.shown_terminal(cx) {
            terminal.update(cx, |terminal, cx| terminal.close_focused(window, cx));
        }
    }

    /// Focuses the pane `step` places from the one focused in this window.
    pub(crate) fn focus_terminal_pane(
        &mut self,
        step: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(terminal) = self.shown_terminal(cx) {
            terminal.update(cx, |terminal, cx| terminal.focus_step(step, window, cx));
        }
    }

    /// Keeps the terminals in step with what this window shows. When the drawer it
    /// shows changes, every pane is fitted again, so a window that stopped showing one
    /// gives up its claim on the PTY's size. Focus left in a pane this window no longer
    /// draws, because it shows another session now or the drawer hid in another
    /// window, moves to the composer, so typing reaches something on screen.
    pub(crate) fn follow_terminals(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(terminals) = cx.try_global::<Terminals>() else {
            return;
        };
        let shown = self.shown_terminal(cx).map(|terminal| terminal.entity_id());
        let hidden = terminals.sessions.values().any(|terminal| {
            Some(terminal.entity_id()) != shown && terminal.read(cx).contains_focus(window)
        });
        if std::mem::replace(&mut self.drawer_shown, shown) != shown {
            cx.defer(refit);
        }
        if hidden {
            self.focus_composer(window, cx);
        }
    }

    /// The window was hidden or shown again. Hidden, it gives up its claim on the
    /// PTY's size; shown, its drawer repaints to claim it back, since the platform
    /// shows the frame it drew before.
    pub(crate) fn terminal_presented(&self, cx: &mut Context<Self>) {
        if let Some(terminal) = self.shown_terminal(cx) {
            terminal.update(cx, |_, cx| cx.notify());
        }
        cx.defer(refit);
    }

    fn main_min_height(&self) -> Pixels {
        self.presentation.current.arrangement.min_height()
    }

    /// The selected session's terminal, while its drawer shows: it is open, and
    /// Settings does not cover it.
    fn shown_terminal(&self, cx: &App) -> Option<Entity<SessionTerminal>> {
        if self.settings.is_some() {
            return None;
        }
        session_terminal(self.selected(cx)?, cx).filter(|terminal| terminal.read(cx).open)
    }

    /// The drawer below the main area, a cached view so typing in the composer does not
    /// repaint the grid. A reused cached view reports no element geometry, so
    /// verification trades that speed for names, as the sidebar does.
    pub(crate) fn terminal_drawer(&self, window: &Window, cx: &App) -> Option<AnyElement> {
        if let Some(machine) = self.terminal_elsewhere(cx) {
            let colors = cx.theme().colors;
            return Some(
                div()
                    .automation_id("terminal-elsewhere")
                    .w_full()
                    .h(MIN_HEIGHT)
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .px(px(24.))
                    .bg(colors.terminal.background)
                    .border_t_1()
                    .border_color(colors.border)
                    .child(
                        Label::new(format!(
                            "The terminal isn't available for sessions on {machine} yet."
                        ))
                        .color(Color::Muted),
                    )
                    .into_any_element(),
            );
        }
        let terminal = self.shown_terminal(cx)?;
        let main = self.main_min_height();
        let height = clamp_height(terminal.read(cx).height, main, window, cx);
        Some(if keystone_ui::automation::enabled() {
            div()
                .w_full()
                .h(height)
                .flex_shrink_0()
                .child(terminal)
                .into_any_element()
        } else {
            gpui::AnyView::from(terminal)
                .cached(
                    StyleRefinement::default()
                        .w_full()
                        .h(height)
                        .flex_shrink_0(),
                )
                .into_any_element()
        })
    }

    /// The top bar's way to open and hide the terminal with the mouse. A narrow window's
    /// bar has no room for it; the shortcut and the palette still work there.
    pub(crate) fn terminal_toggle(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if self.presentation.current.arrangement.fitted {
            return None;
        }
        let session = self.selected(cx)?;
        let open = session_terminal(session, cx).is_some_and(|t| t.read(cx).open)
            || self.terminal_elsewhere(cx).is_some();
        Some(
            IconButton::new("terminal-toggle", IconName::Terminal)
                .size(ButtonSize::Compact)
                .selected(open)
                .debug_selector(|| "terminal-toggle".into())
                // One label for both states: a tooltip stays up across the click that
                // changes the state.
                .tooltip(Tooltip::with_key(
                    "Terminal",
                    crate::commands::hint("toggle-terminal", cx),
                ))
                .on_click(self.scoped_listener(cx, |this, _, window, cx| {
                    match this.shown_terminal(cx) {
                        Some(terminal) => this.hide_terminal(terminal, window, cx),
                        None => this.toggle_terminal(window, cx),
                    }
                })),
        )
    }
}

#[cfg(all(test, feature = "ui-tests"))]
#[path = "terminal_tests/mod.rs"]
mod tests;
