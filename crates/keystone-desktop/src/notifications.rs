//! Desktop notifications, through the platform's notification service
//! (org.freedesktop.Notifications on Linux), for a session the user cannot see: its
//! turn finished, it asks for approval, or its turn failed. A session is seen while an
//! active Keystone window shows it, so nothing is sent for it then. Each session sends
//! each kind once until the user sees it again. Activating one shows its session.
//! Notifications only read events: attention and Needs action are unchanged.

use crate::{DesktopApp, Keystone};
use gpui::{App, Context, SystemNotification, SystemNotificationResponse, Window};
use keystone_agent::{Event, TurnStatus};
use keystone_core::{Notifications, WindowId};
use std::collections::HashMap;

#[cfg(all(test, feature = "ui-tests"))]
#[path = "notifications_tests.rs"]
mod tests;

/// What the user is told about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Alert {
    TurnFinished,
    Approval,
    TurnFailed,
}

impl Alert {
    /// The alert a live provider event raises. A turn the user stopped, or ended with a
    /// denial, raises none: they were there.
    pub(crate) fn of(event: &Event) -> Option<Self> {
        match event {
            Event::ApprovalRequested(_) => Some(Self::Approval),
            Event::TurnCompleted { stopped: true, .. } => None,
            Event::TurnCompleted { status, .. } => Some(match status {
                TurnStatus::Completed => Self::TurnFinished,
                TurnStatus::Interrupted | TurnStatus::Failed => Self::TurnFailed,
            }),
            Event::Failed { .. } => Some(Self::TurnFailed),
            _ => None,
        }
    }

    fn enabled(self, settings: Notifications) -> bool {
        settings.enabled
            && match self {
                Self::TurnFinished => settings.turn_finished,
                Self::Approval => settings.approval,
                Self::TurnFailed => settings.turn_failed,
            }
    }

    fn body(self) -> &'static str {
        match self {
            Self::TurnFinished => "Turn finished",
            Self::Approval => "Approval requested",
            Self::TurnFailed => "Turn failed",
        }
    }
}

/// What has been sent since the user last saw each session, and the window they used
/// last, where a notification for a session no window shows opens it.
#[derive(Default)]
pub(crate) struct Notifier {
    sent: HashMap<u64, Vec<Alert>>,
    last_window: Option<WindowId>,
}

impl Notifier {
    /// Whether `alert` for `session` is new since the user last saw it, recording it.
    fn record(&mut self, session: u64, alert: Alert) -> bool {
        let sent = self.sent.entry(session).or_default();
        if sent.contains(&alert) {
            return false;
        }
        sent.push(alert);
        true
    }

    /// The user sees these sessions, so their next events notify again.
    pub(crate) fn seen(&mut self, sessions: &[u64]) {
        for session in sessions {
            self.sent.remove(session);
        }
    }

    pub(crate) fn unseen(&self, sessions: &[u64]) -> bool {
        sessions.iter().any(|s| self.sent.contains_key(s))
    }

    pub(crate) fn used(&mut self, window: WindowId) {
        self.last_window = Some(window);
    }
}

fn tag(session: u64) -> String {
    format!("session-{session}")
}

/// Names Keystone to the notification service and opens a session from its
/// notification.
pub(crate) fn install(cx: &mut App) {
    cx.set_app_identity("keystone", "Keystone");
    cx.on_system_notification_response(open);
}

/// Sends what `alerts` call for, after the batch that raised them is applied. Runs
/// outside the app state's update, because which sessions a window shows reads it.
pub(crate) fn offer(alerts: Vec<(u64, Alert)>, cx: &mut App) {
    let Some(state) = cx.try_global::<DesktopApp>().map(|app| app.state.clone()) else {
        return;
    };
    let settings = state
        .read(cx)
        .store()
        .workspace()
        .preferences()
        .notifications();
    let seen = cx
        .active_window()
        .and_then(|window| window.downcast::<Keystone>())
        .and_then(|window| window.read(cx).ok())
        .map(|view| view.on_screen(cx))
        .unwrap_or_default();
    for (session, alert) in alerts {
        if !alert.enabled(settings) || seen.contains(&session) {
            continue;
        }
        let Some(title) = state
            .read(cx)
            .session(session)
            .map(|s| s.title().to_owned())
        else {
            continue;
        };
        if state.update(cx, |state, _| state.notifier.record(session, alert)) {
            cx.show_system_notification(SystemNotification {
                tag: tag(session).into(),
                title: title.into(),
                body: alert.body().into(),
                actions: Vec::new(),
            });
        }
    }
}

/// Shows the notification's session and counts its alert as seen.
fn open(response: SystemNotificationResponse, cx: &mut App) {
    let Some(session) = response
        .tag
        .strip_prefix("session-")
        .and_then(|id| id.parse::<u64>().ok())
    else {
        return;
    };
    if let Some(app) = cx.try_global::<DesktopApp>() {
        let state = app.state.clone();
        state.update(cx, |state, _| state.notifier.seen(&[session]));
    }
    show(session, cx);
}

/// Shows `session` in the window showing it, else one holding its project, else the
/// window used last, else the first, and asks the compositor to focus that window.
pub(crate) fn show(session: u64, cx: &mut App) {
    let Some(state) = cx.try_global::<DesktopApp>().map(|app| app.state.clone()) else {
        return;
    };
    let state_now = state.read(cx);
    if state_now.phase != crate::app_state::Phase::Running || state_now.session(session).is_none() {
        return;
    }
    // The last window closed while a turn ran: the user wants Keystone back, so it
    // stays open instead of quitting once idle.
    if crate::open_windows(cx).is_empty() {
        state.update(cx, |state, _| state.quit_when_idle = false);
        crate::windows::reopen(state.clone(), cx);
    }
    let mut windows: Vec<_> = cx
        .windows()
        .into_iter()
        .filter_map(|window| window.downcast::<Keystone>())
        .filter_map(|window| Some((window, window.read(cx).ok()?.window_id)))
        .collect();
    windows.sort_by_key(|(_, id)| *id);
    let last = state.read(cx).notifier.last_window;
    let holding: Vec<_> = windows
        .iter()
        .filter(|(window, _)| {
            window
                .read(cx)
                .is_ok_and(|view| view.project_for(session, cx).is_none())
        })
        .collect();
    let target = windows
        .iter()
        .find(|(window, _)| {
            window
                .read(cx)
                .is_ok_and(|view| view.on_screen(cx).contains(&session))
        })
        .or_else(|| holding.iter().copied().find(|(_, id)| Some(*id) == last))
        .or_else(|| holding.first().copied())
        .or_else(|| windows.iter().find(|(_, id)| Some(*id) == last))
        .or(windows.first());
    if let Some((window, _)) = target {
        let _ = window.update(cx, |view, window, cx| {
            view.open_notified(session, window, cx)
        });
    }
}

impl Keystone {
    /// The sessions whose conversation this window drew in its last frame, none while
    /// the settings page or the narrow window's sidebar covers them.
    pub(crate) fn on_screen(&self, cx: &App) -> Vec<u64> {
        let arrangement = &self.presentation.current.arrangement;
        if self.settings.is_some()
            || (arrangement.sidebar_overlay
                && arrangement.fitted
                && self.arrangement_has_sidebar(cx))
        {
            return Vec::new();
        }
        let tabs = self.session_tabs(cx);
        let selected = self.selected(cx);
        let mut shown: Vec<u64> = self
            .presentation
            .current
            .arrangement
            .visible
            .iter()
            .filter_map(|&view| tabs.get(view).or(selected))
            .collect();
        shown.sort_unstable();
        shown.dedup();
        shown
    }

    /// While this window is active, what it shows is seen. Runs once the frame's
    /// visible views are known.
    pub(crate) fn note_seen(&mut self, window: &Window, cx: &mut Context<Self>) {
        if !window.is_window_active() {
            return;
        }
        let shown = self.on_screen(cx);
        if self.state.read(cx).notifier.unseen(&shown) {
            self.state
                .update(cx, |state, _| state.notifier.seen(&shown));
        }
    }

    /// Selects `session` from its notification, opening its project when this window shows
    /// another, leaving Settings and Customize as opening Settings does, and focuses the
    /// composer and the window. A refused project switch selects nothing and shows its error.
    fn open_notified(&mut self, session: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.settings = None;
        if let Some(project) = self.project_for(session, cx) {
            self.open_project(project, cx);
            if self.project_for(session, cx).is_some() {
                window.activate_window();
                return;
            }
            self.focus_project_transition(window, cx);
        }
        if self.presentation.current.customization.is_open() {
            self.customize(crate::arrangement_editor::Action::Close, window, cx);
        }
        // Selecting again brings the conversation forward when Files hides it.
        self.select(session, cx);
        // Focus left in another pane would take the selection back.
        self.focus_composer(window, cx);
        self.refresh(cx);
        window.activate_window();
    }
}
