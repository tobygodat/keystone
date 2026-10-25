//! Several windows on one workspace. Each window has its own tabs, panes and
//! selection, and its composer sends to the session it shows. Sessions, turns,
//! accounts and recovery belong to the app, so closing a window leaves its work
//! running, and every window's sidebar reaches it.

use crate::session_tabs::{bring_forward, conversations, replacement};
use crate::{AppState, Keystone, durability_warning, open_windows, register_window};
use gpui::{
    App, AppContext, Bounds, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    Pixels, Point, Styled, Window, WindowBounds, WindowOptions, anchored, deferred, point,
    prelude::FluentBuilder, px, size,
};
use keystone_core::{
    Axis, Component, Composition, Edit, Place, Placement, Size, ViewId, WindowId, WindowView,
};
use keystone_ui::prelude::*;
use std::num::NonZeroU16;

gpui::actions!(
    keystone,
    [NewWindow, OpenInNewWindow, CloseWindow, MoveTabToWindow]
);

/// The size a window opens at when nothing was saved for it.
const DEFAULT_SIZE: (f32, f32) = (1100., 720.);
/// The smallest saved size a window is restored at.
const MIN_SIZE: f32 = 320.;
/// How far a new window opens from the one it came from, where the compositor lets
/// a client choose.
const CASCADE: f32 = 32.;

/// The menu a right click on a sidebar session or a session tab opens.
pub(crate) struct ContextMenu {
    session: u64,
    position: Point<Pixels>,
}

/// What the window list and title bar call `id`.
pub(crate) fn title(id: WindowId) -> String {
    if id == WindowId::FIRST {
        "Keystone".into()
    } else {
        format!("Keystone {}", id.0 + 1)
    }
}

fn bounds(placement: Option<Placement>, cx: &App) -> WindowBounds {
    let default = || {
        WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(DEFAULT_SIZE.0), px(DEFAULT_SIZE.1)),
            cx,
        ))
    };
    let Some(p) = placement.filter(|p| [p.x, p.y, p.width, p.height].iter().all(|n| n.is_finite()))
    else {
        return default();
    };
    let bounds = Bounds {
        origin: point(px(p.x), px(p.y)),
        size: size(px(p.width.max(MIN_SIZE)), px(p.height.max(MIN_SIZE))),
    };
    if p.maximized {
        WindowBounds::Maximized(bounds)
    } else {
        WindowBounds::Windowed(bounds)
    }
}

/// Opens saved window `id` where it was, showing what it saved.
pub(crate) fn open(
    state: Entity<AppState>,
    id: WindowId,
    placement: Option<Placement>,
    launch: bool,
    fixture: bool,
    cx: &mut App,
) -> anyhow::Result<()> {
    cx.open_window(
        WindowOptions {
            window_bounds: Some(bounds(placement, cx)),
            titlebar: None,
            app_id: Some("keystone".into()),
            ..Default::default()
        },
        |window, cx| {
            let view = cx.new(|cx| {
                let view = Keystone::in_window(state, id, launch, fixture, window, cx);
                view.focus.focus(window, cx);
                view
            });
            register_window(view.downgrade(), window, cx);
            view
        },
    )?;
    Ok(())
}

/// Where a tab moved from another window lands.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Landing {
    /// In the strip of the pane showing `anchor`, before the tab at `index`.
    Strip { anchor: ViewId, index: usize },
    /// In a new pane beside `target`.
    Beside {
        target: ViewId,
        axis: Axis,
        after: bool,
    },
    /// In a new tab beside the active view.
    Active,
}

/// Where a moving tab goes.
pub(crate) enum Destination {
    Window(Entity<Keystone>, Landing),
    NewWindow,
}

/// Moves the tab `view` of `source` to `destination`, saving the window it leaves and
/// the one it reaches in one write. The session keeps running; only where it shows
/// changes. Nothing changes when either window cannot take part, and the error shows in
/// `asker`, the window where the move was asked for.
pub(crate) fn move_tab(
    source: &Entity<Keystone>,
    view: ViewId,
    destination: Destination,
    asker: &Entity<Keystone>,
    cx: &mut App,
) {
    let fail = |error: String, cx: &mut App| {
        asker.update(cx, |this, cx| {
            this.error = Some(format!("Tab not moved. {error}"));
            this.refresh(cx);
        })
    };
    if !source.update(cx, |source, cx| source.save_draft(cx)) {
        return;
    }
    let (from, session) = match source.read(cx).view_without(view, cx) {
        Ok(left) => left,
        Err(error) => return fail(error, cx),
    };
    let state = source.read(cx).state.clone();
    match destination {
        Destination::Window(target, landing) => {
            if target == *source {
                return;
            }
            if target.read(cx).presentation.project.as_ref().ok() != Some(&from.project) {
                return fail(
                    format!(
                        "{} shows another project.",
                        title(target.read(cx).window_id)
                    ),
                    cx,
                );
            }
            if !target.update(cx, |target, cx| target.save_draft(cx)) {
                return;
            }
            let to = match target.update(cx, |target, cx| target.view_with(session, landing, cx)) {
                Ok(to) => to,
                Err(error) => return fail(error, cx),
            };
            let shown = to.layout.as_ref().map(|_| to.tabs.view_of(session));
            match state.update(cx, |state, cx| state.move_tab(from, to, cx)) {
                Ok(applied) => {
                    source.update(cx, |source, cx| source.tab_left(session, cx));
                    target.update(cx, |target, cx| {
                        target.error = durability_warning(applied.durability);
                        target.tab_arrived(shown.flatten(), cx);
                    });
                }
                Err(error) => fail(error.to_string(), cx),
            }
        }
        Destination::NewWindow => {
            let handle = source.read(cx).handle;
            let Ok(mut placement) = handle.update(cx, |_, window, _| crate::placement(window))
            else {
                return;
            };
            placement.x += CASCADE;
            placement.y += CASCADE;
            placement.maximized = false;
            match state.update(cx, |state, cx| {
                state.move_tab_to_new_window(from, session, Some(placement), cx)
            }) {
                Ok(applied) => {
                    let fixture = source.update(cx, |source, cx| {
                        source.error = durability_warning(applied.durability);
                        source.tab_left(session, cx);
                        source.fixture
                    });
                    if let Err(error) =
                        open(state, applied.value, Some(placement), false, fixture, cx)
                    {
                        source.update(cx, |source, cx| {
                            source.error = Some(format!(
                                "Window not opened; it opens at the next launch. {error}"
                            ));
                            source.refresh(cx);
                        });
                    }
                }
                Err(error) => fail(error.to_string(), cx),
            }
        }
    }
}

/// Opens every saved window again, after the last one closed and the Quit that
/// followed could not save, so its error shows and Quit can be retried.
pub(crate) fn reopen(state: Entity<AppState>, cx: &mut App) {
    let saved = state.read(cx).store().workspace().preferences().windows();
    for (id, placement) in saved {
        if let Err(error) = open(state.clone(), id, placement, false, false, cx) {
            eprintln!("Could not reopen a Keystone window: {error}");
        }
    }
}

impl Keystone {
    /// Opens a new window showing `selected`, or the welcome composer. It is saved
    /// before it opens, so a crash right after still brings it back.
    pub(crate) fn open_new_window(
        &mut self,
        selected: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.context_menu = None;
        if !self.save_draft(cx) {
            return;
        }
        let mut placement = crate::placement(window);
        placement.x += CASCADE;
        placement.y += CASCADE;
        placement.maximized = false;
        let project = self.presentation.project.as_ref().ok().cloned();
        let opened = self.state.update(cx, |state, _| {
            state.open_window(project.as_ref(), selected, Some(placement))
        });
        let id = match opened {
            Ok(applied) => {
                self.error = durability_warning(applied.durability);
                applied.value
            }
            Err(error) => {
                self.error = Some(format!("Window not opened. {error}"));
                return self.refresh(cx);
            }
        };
        let (state, fixture) = (self.state.clone(), self.fixture);
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            let Err(error) = open(state.clone(), id, Some(placement), false, fixture, cx) else {
                return;
            };
            let _ = state.update(cx, |state, cx| state.close_window(id, cx));
            let _ = this.update(cx, |this, cx| {
                this.error = Some(format!("Window not opened. {error}"));
                this.refresh(cx);
            });
        });
        self.refresh(cx);
    }

    /// A tab dropped here from another window: it moves here, out of that one.
    pub(crate) fn receive_tab(
        &mut self,
        drag: &keystone_ui::DraggedTab,
        landing: Landing,
        cx: &mut Context<Self>,
    ) {
        let target = cx.entity();
        let Some(source) = open_windows(cx)
            .into_iter()
            .filter(|view| *view != target)
            .find(|view| view.read(cx).handle.window_id() == drag.window)
        else {
            return;
        };
        let Some(view) = source.read(cx).dragged_view(drag) else {
            return;
        };
        cx.defer(move |cx| {
            move_tab(
                &source,
                view,
                Destination::Window(target.clone(), landing),
                &target,
                cx,
            )
        });
    }

    /// What this window shows once `view` leaves it, and the session it held. The pane
    /// closes with its last tab and its split collapses, as when the tab closes; the
    /// last conversation view stays and follows the selection again. The selection
    /// moves to the tab shown in its place, or to none, so this window stops showing
    /// the session.
    fn view_without(&self, view: ViewId, cx: &App) -> Result<(WindowView, u64), String> {
        if self.presentation.current.customization.is_open() {
            return Err("Close Customize first.".into());
        }
        let project = self
            .presentation
            .project
            .clone()
            .map_err(|error| format!("Cannot save this project's layout. {error}"))?;
        let mut tabs = self.session_tabs(cx);
        let selected = self.selected(cx);
        let session = tabs
            .get(view)
            .or(selected.filter(|_| self.active_view(cx) == Some(view)))
            .ok_or("That tab shows no session.")?;
        let before = self.saved_composition(cx);
        let views = conversations(&before);
        let (layout, shown) = if views.len() < 2 {
            tabs.release(view);
            (None, None)
        } else {
            let after = before.edit(Edit::Close(view)).map_err(|p| p.to_string())?;
            tabs.retain(|v, _| after.component(v) == Some(Component::Conversation));
            let shown = replacement(&before, &after, view)
                .filter(|_| self.active_view(cx) == Some(view))
                .and_then(|shown| tabs.get(shown));
            (Some(after), shown)
        };
        let selected = shown.or(selected.filter(|&s| s != session));
        Ok((
            WindowView {
                window: self.window_id,
                project,
                layout,
                tabs,
                selected,
            },
            session,
        ))
    }

    /// What this window shows once `session` arrives at `landing`, selected in a new
    /// tab, or in the tab that already holds it.
    fn view_with(
        &mut self,
        session: u64,
        landing: Landing,
        cx: &mut Context<Self>,
    ) -> Result<WindowView, String> {
        let project = self
            .presentation
            .project
            .clone()
            .map_err(|error| format!("Cannot save this project's layout. {error}"))?;
        let (c, mut tabs, active) = self.layout_for_change(cx).ok_or("Close Customize first.")?;
        if let Some(held) = tabs.view_of(session) {
            return Ok(WindowView {
                window: self.window_id,
                project,
                layout: bring_forward(&c, held),
                tabs,
                selected: Some(session),
            });
        }
        let view = self.presentation.current.customization.allocate()?;
        let open = |at| Edit::Open {
            view,
            component: Component::Conversation,
            at,
            size: Size::Fill(NonZeroU16::MIN),
        };
        let layout: Result<Composition, _> = match landing {
            Landing::Strip { anchor, index } => c
                .edit(open(Place::TabWith(anchor)))
                .and_then(|c| c.edit(Edit::Reorder { view, index }))
                .and_then(|c| c.edit(Edit::SelectTab(view))),
            Landing::Beside {
                target,
                axis,
                after,
            } => c.edit(open(Place::Beside {
                target,
                axis,
                after,
            })),
            Landing::Active => c
                .edit(open(Place::TabWith(active)))
                .and_then(|c| c.edit(Edit::SelectTab(view))),
        };
        tabs.hold(view, session);
        Ok(WindowView {
            window: self.window_id,
            project,
            layout: Some(layout.map_err(|p| p.to_string())?),
            tabs,
            selected: Some(session),
        })
    }

    /// Catches up with a tab that left this window, saved by [`move_tab`].
    fn tab_left(&mut self, session: u64, cx: &mut Context<Self>) {
        self.replies.get_mut().forget(session);
        self.presentation.current.customization.forget_undo();
        self.presentation.selection_changed();
        self.sync_draft(cx);
        self.refresh(cx);
    }

    /// Catches up with a tab that arrived in this window as `view`, saved by
    /// [`move_tab`].
    fn tab_arrived(&mut self, view: Option<ViewId>, cx: &mut Context<Self>) {
        if let Some(view) = view {
            self.show_fitted(view, cx);
        }
        self.presentation.current.customization.forget_undo();
        self.presentation.selection_changed();
        self.sync_draft(cx);
        self.refresh(cx);
    }

    pub(crate) fn open_context_menu(
        &mut self,
        session: u64,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.context_menu = Some(ContextMenu { session, position });
        cx.notify();
    }

    /// Keeps the window list's title in step with which saved window this is.
    pub(crate) fn sync_title(&mut self, window: &mut Window) {
        if self.titled != Some(self.window_id) {
            window.set_window_title(&title(self.window_id));
            self.titled = Some(self.window_id);
        }
    }

    pub(crate) fn context_menu_overlay(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let menu = self.context_menu.as_ref()?;
        let session = menu.session;
        let card = v_flex()
            .id("context-menu")
            .occlude()
            .min_w(px(200.))
            .p(px(4.))
            .gap(px(2.))
            .surface(Elevation::Overlay, cx)
            .on_mouse_down_out(self.scoped_listener(cx, |this, _, _, cx| {
                this.context_menu = None;
                cx.notify();
            }))
            .child(
                MenuItem::new("open-in-new-window", "Open in new window")
                    .icon(IconName::Plus)
                    .debug_selector(|| "menu-open-in-new-window".into())
                    .on_click(self.scoped_listener(cx, move |this, _, window, cx| {
                        this.open_new_window(Some(session), window, cx)
                    })),
            )
            .when(
                self.session(session, cx)
                    .is_some_and(|s| s.worktree().is_some()),
                |card| {
                    card.child(
                        MenuItem::new("remove-worktree", "Remove worktree…")
                            .icon(IconName::Branch)
                            .debug_selector(|| "menu-remove-worktree".into())
                            .on_click(self.scoped_listener(cx, move |this, _, window, cx| {
                                this.open_remove_worktree(session, window, cx)
                            })),
                    )
                },
            );
        Some(
            deferred(
                anchored()
                    .position(menu.position)
                    .snap_to_window_with_margin(px(8.))
                    .child(card),
            )
            .priority(2),
        )
    }
}

#[cfg(all(test, feature = "ui-tests"))]
#[path = "windows_tests.rs"]
mod tests;
