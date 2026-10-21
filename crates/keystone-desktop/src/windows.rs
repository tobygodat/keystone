//! Several windows on one workspace. Each window has its own tabs, panes and
//! selection, and its composer sends to the session it shows. Sessions, turns,
//! accounts and recovery belong to the app, so closing a window leaves its work
//! running, and every window's sidebar reaches it.

use crate::{AppState, Keystone, durability_warning, register_window};
use gpui::{
    App, AppContext, Bounds, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    Pixels, Point, Styled, Window, WindowBounds, WindowOptions, anchored, deferred, point, px,
    size,
};
use keystone_core::{Placement, WindowId};
use keystone_ui::prelude::*;

gpui::actions!(keystone, [NewWindow, OpenInNewWindow, CloseWindow]);

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
