mod app_state;
use app_state::{AppState, RuntimeChanged};
mod arrangement;
mod arrangement_editor;
mod attachments;
mod changes;
mod commands;
mod conversation;
mod customization;
mod customization_agent;
mod files;
mod handoff;
mod integrations;
mod mentions;
mod model_selector;
mod notes;
mod notifications;
mod presentation;
mod projects;
mod prompts;
mod relative_time;
mod reveal;
mod session_tabs;
mod settings;
mod settings_accounts;
mod settings_page;
mod settings_prompts;
mod slash_menu;
mod transcript;
mod transcript_keyboard;
mod usage;
mod view_state;
mod widget;
mod windows;
mod worktrees;
use conversation::{Conversation, Row};
use futures::StreamExt;
use gpui::{
    Anchor, AnyElement, AnyView, App, AvailableSpace, BackgroundExecutor, BoxShadow, Context,
    Entity, ExternalPaths, FocusHandle, Focusable, FontWeight, IntoElement, ListOffset,
    MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Render, ScrollHandle, StyleRefinement,
    Subscription, WeakEntity, Window, WindowBounds, canvas, div, list, point, prelude::*, px, size,
};
use keystone_agent::log::{Logs, Report};
use keystone_agent::{
    ApprovalState, ClaudeConnection, CodexConnection, Command, Connection, Event, ItemKind,
    RequestId, TurnStatus, Wake,
};
#[cfg(test)]
use keystone_core::ViewId;
#[cfg(test)]
use keystone_core::WorkingDirectory;
use keystone_core::{
    AccountId, Appearance, ApprovalMode, Attention, Durability, Provider, Revision, SidebarSide,
    Store,
};
use keystone_ui::markdown::Document;
use keystone_ui::text_input::{Changed, SelectAll};
use keystone_ui::{automation, prelude::*};
use model_selector::{
    CatalogKey, Catalogs, Choices, provider_icon, provider_icon_color, provider_name,
};
use presentation::Presentation;
use relative_time::{LocalTime, relative};
use settings_page::SettingsPage;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
#[cfg(all(test, feature = "ui-tests"))]
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use view_state::ViewKey;

gpui::actions!(
    keystone,
    [
        Quit,
        NewClaude,
        NewCodex,
        ToggleSettle,
        FocusComposer,
        FocusSearch,
        SendMessage,
        RenameSession,
        NextFocus,
        PreviousFocus,
        ToggleSidebar,
        OpenPalette,
        OpenSettings,
        ContinueRecovery,
        RetryRecovery
    ]
);

/// The transcript and the composer share one centred column.
const CONTENT_WIDTH: f32 = 640.;

/// Keystone's mark: two muted side stones and the keystone in the accent colour.
fn arch_mark(size: Pixels) -> impl IntoElement {
    div()
        .relative()
        .flex_shrink_0()
        .size(size)
        .child(Icon::new(IconName::ArchStones).size(size))
        .child(
            div().absolute().top_0().left_0().child(
                Icon::new(IconName::ArchKeystone)
                    .size(size)
                    .color(Color::Accent),
            ),
        )
}

fn ranges(mut rows: Vec<usize>) -> Vec<Range<usize>> {
    rows.sort_unstable();
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for index in rows {
        match ranges.last_mut() {
            Some(range) if range.end == index => range.end += 1,
            _ => ranges.push(index..index + 1),
        }
    }
    ranges
}

/// The wall clock in unix milliseconds. The store takes it as an argument, so its
/// tests pass fixed times.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Whether `event` ends a turn, and if so whether the turn failed. An interruption is a
/// normal end only when the user ended that turn, which the provider reports with it.
fn turn_failed(event: &Event) -> Option<bool> {
    match event {
        Event::TurnCompleted {
            status, stopped, ..
        } => Some(match status {
            TurnStatus::Completed => false,
            TurnStatus::Interrupted => !stopped,
            TurnStatus::Failed => true,
        }),
        Event::Failed { .. } => Some(true),
        _ => None,
    }
}

/// How long closing waits for saved histories to finish loading.
const LOAD_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

const FOLDER_UNCONFIRMED: &str =
    "Folder selected. Could not confirm it will survive a system crash. Send will retry saving.";

fn durability_warning(durability: Durability) -> Option<String> {
    match durability {
        Durability::Synced => None,
        Durability::DirectorySyncPending(error) => Some(format!(
            "Change saved, but crash durability is unconfirmed. Retry saving before leaving. {error}"
        )),
    }
}

/// The open rename dialog. A failed save keeps it open with `error`.
struct Renaming {
    id: u64,
    focus: DialogFocus,
    error: Option<String>,
}

struct FullAccessConfirmation {
    session: u64,
    activation: presentation::Activation,
    identity: Rc<()>,
    focus: DialogFocus,
}

struct ComposerDraft {
    session: u64,
    text: String,
    attachments: Vec<keystone_core::Attachment>,
}

struct FolderWarning {
    session: u64,
    revision: Revision,
}

struct DesktopApp {
    state: Entity<AppState>,
    /// Every open Keystone window, in the order they opened.
    windows: Vec<WeakEntity<Keystone>>,
}

impl gpui::Global for DesktopApp {}

fn install_app_state(store: Store, cx: &mut App) -> Entity<AppState> {
    let state = cx.new(|cx| AppState::new(store, cx));
    cx.set_quit_mode(gpui::QuitMode::Explicit);
    notifications::install(cx);
    cx.set_global(DesktopApp {
        state: state.clone(),
        windows: Vec::new(),
    });
    cx.on_action(|_: &Quit, cx| request_quit(cx));
    state
}

fn register_window(root: WeakEntity<Keystone>, window: &Window, cx: &mut App) {
    cx.global_mut::<DesktopApp>().windows.push(root.clone());
    window.on_window_should_close(cx, move |window, cx| close_window(&root, window, cx));
}

/// The open Keystone windows, without any that closed since.
fn open_windows(cx: &App) -> Vec<Entity<Keystone>> {
    cx.try_global::<DesktopApp>()
        .map(|app| app.windows.iter().filter_map(WeakEntity::upgrade).collect())
        .unwrap_or_default()
}

/// Where `window` is and how big, to save and to restore.
fn placement(window: &Window) -> keystone_core::Placement {
    let (bounds, maximized) = match window.window_bounds() {
        WindowBounds::Windowed(bounds) => (bounds, false),
        WindowBounds::Maximized(bounds) | WindowBounds::Fullscreen(bounds) => (bounds, true),
    };
    keystone_core::Placement {
        x: f32::from(bounds.origin.x),
        y: f32::from(bounds.origin.y),
        width: f32::from(bounds.size.width),
        height: f32::from(bounds.size.height),
        maximized,
    }
}

/// Closes one window and leaves its sessions and their turns running. Another window
/// keeps the closed one's draft. Closing the last window quits as Quit does, at once
/// when no turn runs, else once the last turn ends, and keeps the window saved so the
/// next launch opens it. Returns whether the window may close now.
fn close_window(root: &WeakEntity<Keystone>, window: &mut Window, cx: &mut App) -> bool {
    let Some(app) = cx.try_global::<DesktopApp>() else {
        return true;
    };
    let state = app.state.clone();
    if state.read(cx).phase != app_state::Phase::Running {
        return false;
    }
    let Some(view) = root.upgrade() else {
        return true;
    };
    let others = open_windows(cx)
        .into_iter()
        .filter(|other| other != &view)
        .count();
    if others == 0 && state.read(cx).awaiting_answer() {
        view.update(cx, |view, cx| {
            view.error = Some(
                "Answer the approval request before closing the last window, or Quit to stop the turn."
                    .into(),
            );
            view.refresh(cx);
        });
        return false;
    }
    if others == 0 && !state.read(cx).turn_running() {
        request_quit(cx);
        return false;
    }
    let id = view.read(cx).window_id;
    let placement = placement(window);
    let saved = view.update(cx, |view, cx| {
        view.draft_save_task = None;
        let composer = view.save_draft(cx);
        if let Err(error) = view.changes.save_drafts(cx) {
            return Err(Some(format!(
                "Window not closed. Its commit message is not saved. {error}"
            )));
        }
        if !composer || !view.save_notes(cx) {
            return Err(None);
        }
        let project = view.presentation.project.clone().ok();
        let placed = view.state.update(cx, |state, _| {
            state.set_placement(id, placement, project.as_ref())
        });
        if others == 0 {
            // Quit cannot save a window that is already closed, so it stays open.
            return placed.map(|_| None).map_err(|error| {
                Some(format!(
                    "Window not closed. Its place is not saved; Quit to stop the turn. {error}"
                ))
            });
        }
        view.state
            .update(cx, |state, cx| state.close_window(id, cx))
            .map(|applied| applied.value)
            .map_err(|error| Some(format!("Window not closed. {error}")))
    });
    let promoted = match saved {
        Ok(promoted) => promoted,
        Err(error) => {
            view.update(cx, |view, cx| {
                if error.is_some() {
                    view.error = error;
                }
                view.refresh(cx);
            });
            return false;
        }
    };
    if others == 0 {
        state.update(cx, |state, _| state.quit_when_idle = true);
    }
    view.update(cx, |view, _| view.customize_agent = None);
    cx.global_mut::<DesktopApp>()
        .windows
        .retain(|w| w.upgrade().is_some_and(|w| w != view));
    automation::forget_window(window.window_handle().window_id().as_u64());
    if let Some(heir) = promoted {
        for other in open_windows(cx) {
            other.update(cx, |other, cx| {
                if other.window_id == heir {
                    other.window_id = keystone_core::WindowId::FIRST;
                    other.refresh(cx);
                }
            });
        }
    }
    true
}

fn request_quit(cx: &mut App) {
    let Some(app) = cx.try_global::<DesktopApp>() else {
        return;
    };
    let state = app.state.clone();
    if !state.update(cx, |state, _| state.reserve_quit()) {
        return;
    }
    cx.defer(move |cx| {
        let windows = open_windows(cx);
        let mut drafts = Vec::new();
        let mut notes = Vec::new();
        let mut saved = Vec::new();
        let mut unsaved = None;
        for view in &windows {
            let handle = view.read(cx).handle;
            let id = view.read(cx).window_id;
            let draft = view.update(cx, |view, cx| {
                view.draft_save_task = None;
                view.composer_draft(cx)
            });
            if let Err(error) = view.update(cx, |view, cx| view.changes.save_drafts(cx)) {
                unsaved.get_or_insert(error);
            }
            drafts.extend(draft.filter(|d| {
                drafts
                    .iter()
                    .all(|o: &ComposerDraft| o.session != d.session)
            }));
            for note in view.read(cx).unsaved_notes(cx) {
                if notes.iter().all(|(project, _)| project != &note.0) {
                    notes.push(note);
                }
            }
            let project = view.read(cx).presentation.project.clone().ok();
            if let Ok(placement) = handle.update(cx, |_, window, _| placement(window)) {
                saved.push((id, placement, project));
            }
        }
        if state
            .update(cx, |state, cx| state.save(drafts, notes, saved, cx))
            .is_err()
        {
            if windows.is_empty() {
                windows::reopen(state, cx);
            }
            return;
        }
        if let Some(error) = unsaved {
            state.update(cx, |state, cx| {
                state.quit_failed(format!("A commit message is not saved. {error}"), cx)
            });
            return;
        }
        state.update(cx, |state, cx| state.teardown(cx));
        for view in windows {
            view.update(cx, |view, _| view.customize_agent = None);
        }
        cx.quit();
    });
}

struct Keystone {
    state: Entity<AppState>,
    /// Which saved window this is. Its layout, tabs and selection are its own.
    window_id: keystone_core::WindowId,
    handle: gpui::AnyWindowHandle,
    /// The menu a right click on a session or a tab opened.
    context_menu: Option<windows::ContextMenu>,
    placement_task: Option<gpui::Task<()>>,
    /// The saved window the platform title names, set once it has been.
    titled: Option<keystone_core::WindowId>,
    error: Option<String>,
    folder_warning: Option<FolderWarning>,
    composer: Entity<TextInput>,
    search: Entity<TextInput>,
    session_sidebar: Entity<SessionSidebar>,
    _subscriptions: Vec<Subscription>,
    draft_save_task: Option<gpui::Task<()>>,
    focus: FocusHandle,
    /// The recovery card's Continue and Retry buttons, in the view the composer sends
    /// to, so Tab reaches them.
    recovery_focus: (FocusHandle, FocusHandle),
    new_menu: bool,
    filter_menu: bool,
    provider_filter: Option<Provider>,
    settings: Option<SettingsPage>,
    approval_menu: bool,
    /// The session whose model menu is open. It closes with that session's first send,
    /// with a new session, and with a selection change, so it never shows for another.
    model_menu: Option<u64>,
    /// Full access chosen from the menu for this session, waiting in the confirmation
    /// dialog. Nothing is saved or sent until Confirm.
    confirm_full_access: Option<FullAccessConfirmation>,
    renaming: Option<Renaming>,
    rename_input: Entity<TextInput>,
    worktree_dialog: Option<worktrees::WorktreeDialog>,
    worktree_input: Entity<TextInput>,
    /// Set by a press on the top bar. The first move then hands the window to the
    /// compositor, so a plain click never starts a move.
    window_drag: bool,
    /// Whether the sidebar's Settled section is expanded. It starts collapsed.
    settled_open: bool,
    fixture: bool,
    /// The shared Codex app-server connection, created on the first message.
    /// Claude Code processes, one per session, created on the first message.
    /// Each session's log of completed items, read and written off the UI thread.
    presentation: Presentation,
    customize_wake: Wake,
    /// Scrolls the sample transcript, which is not a list.
    transcript_scroll: ScrollHandle,
    /// Each reply row's Markdown and how much of it is shown. The open session's
    /// streaming replies catch up with each burst over a few frames.
    replies: RefCell<reveal::Replies>,
    reveal_task: Option<gpui::Task<()>>,
    clock: BackgroundExecutor,
    /// The sample transcript, parsed once.
    fixture_document: Arc<Document>,
    customize_agent: Option<customization_agent::Agent>,
    palette: Option<commands::Palette>,
    /// The open project list.
    projects: Option<projects::ProjectList>,
    prompt_list: Option<prompts::PromptList>,
    files: files::Files,
    changes: changes::Changes,
    notes: notes::Notes,
    slash: slash_menu::SlashMenu,
    mentions: mentions::Mentions,
    attach: attachments::Attach,
    usage_focus: FocusHandle,
}

impl Keystone {
    fn runtime_changed(&mut self, changed: &RuntimeChanged, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        let now = self.clock.now();
        for change in &changed.rows {
            let Some(conversation) = state.conversations().get(&change.session) else {
                continue;
            };
            self.presentation.invalidate(
                change.session,
                conversation.rows.len().max(change.rows),
                &change.changed,
                true,
            );
            if !self.showing(change.session, cx).is_empty() {
                for &(index, bytes, status) in &change.replies {
                    if let Some(Row::Item(item)) = conversation.rows.get(index)
                        && item.kind == ItemKind::AgentMessage
                    {
                        self.replies.get_mut().observe_text(
                            (change.session, index),
                            item.text.get(..bytes).unwrap_or(&item.text),
                            status,
                            now,
                        );
                    }
                }
            }
        }
        self.agent_changed_files(&changed.files, cx);
        for (folder, text) in &changed.commit_drafts {
            self.changes.sync_draft(folder, text, cx);
        }
        self.reveal(cx);
        if changed.sidebar {
            self.refresh(cx);
        } else {
            cx.notify();
        }
    }
    fn send_message(&mut self, draft: ComposerDraft, window: &mut Window, cx: &mut Context<Self>) {
        if self.slash_blocks(&draft, cx) || self.importing(draft.session) {
            return;
        }
        let id = draft.session;
        let sent = self
            .state
            .update(cx, |state, cx| state.send_message(draft, cx));
        if self.state.read(cx).session(id).is_some_and(|s| s.started()) {
            self.model_menu = None;
        }
        if !sent {
            return;
        }
        self.composer
            .update(cx, |input, cx| input.set_value(String::new(), cx));
        self.save_draft(cx);
        if let Some(key) = self
            .presentation
            .current
            .views
            .send_target(&self.showing(id, cx), id)
        {
            self.presentation.current.views.scroll_to_end(key);
        }
        self.composer.focus_handle(cx).focus(window, cx);
        self.refresh(cx);
    }
    fn stop_turn(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.composer.focus_handle(cx).focus(window, cx);
        self.state.update(cx, |state, cx| state.stop_turn(id, cx));
    }
    fn answer(
        &mut self,
        id: u64,
        request: RequestId,
        allow: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.composer.focus_handle(cx).focus(window, cx);
        self.state
            .update(cx, |state, cx| state.answer(id, request, allow, cx));
    }
    /// Lets the next message go after a partial recovery, and returns to the composer
    /// with the draft as it was.
    fn continue_recovery(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.state
            .update(cx, |state, cx| state.continue_after_recovery(id, cx));
        self.composer.focus_handle(cx).focus(window, cx);
    }
    /// The session this window shows and its composer sends to: its saved selection,
    /// while that is one of the window's project's sessions.
    fn selected(&self, cx: &App) -> Option<u64> {
        let state = self.state.read(cx);
        let selected = state.store().workspace().selected_in(self.window_id)?;
        match (&self.presentation.project, state.session(selected)) {
            (Ok(project), Some(session)) if !projects::holds(project, session) => None,
            _ => Some(selected),
        }
    }
    fn selected_session<'a>(&self, cx: &'a App) -> Option<&'a keystone_core::Session> {
        self.state.read(cx).session(self.selected(cx)?)
    }
    fn session<'a>(&self, id: u64, cx: &'a App) -> Option<&'a keystone_core::Session> {
        self.state.read(cx).session(id)
    }
    fn provider(&self, id: u64, cx: &App) -> Option<Provider> {
        self.state.read(cx).provider(id)
    }
    fn model_selection<'a>(
        &self,
        id: u64,
        provider: Provider,
        cx: &'a App,
    ) -> model_selector::Selection<'a> {
        self.state.read(cx).model_selection(id, provider)
    }
    fn request_catalog(&mut self, id: u64, provider: Provider, cx: &mut Context<Self>) {
        self.state
            .update(cx, |state, cx| state.request_catalog(id, provider, cx));
    }
    fn retry_catalog(&mut self, id: u64, provider: Provider, cx: &mut Context<Self>) {
        self.state
            .update(cx, |state, cx| state.retry_catalog(id, provider, cx));
    }
    fn conversation_folder(&self, id: u64, cx: &App) -> Option<presentation::ConversationFolder> {
        self.state.read(cx).conversation_folder(id)
    }
    fn folder_error(&mut self, id: u64, error: String, cx: &mut Context<Self>) {
        self.state
            .update(cx, |state, cx| state.folder_error(id, error, cx));
    }
    #[cfg(test)]
    fn pump(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| state.pump(cx));
    }
    #[cfg(test)]
    fn receive(&mut self, reports: Vec<Report>, events: Vec<(u64, Event)>, cx: &mut Context<Self>) {
        self.state
            .update(cx, |state, cx| state.receive(reports, events, cx));
    }
    #[cfg(test)]
    fn commit(&mut self, id: u64, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.commit(id);
            state.publish(cx);
        });
    }
    #[cfg(test)]
    fn flush_logs(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = self.state.update(cx, |state, cx| state.flush_logs(cx)) {
            self.error = Some(error);
            cx.notify();
        }
    }

    #[cfg(test)]
    fn new(
        state: Entity<AppState>,
        fixture: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::in_window(
            state,
            keystone_core::WindowId::FIRST,
            true,
            fixture,
            window,
            cx,
        )
    }

    /// `launch` is true for the windows Keystone opens as it starts.
    fn in_window(
        state: Entity<AppState>,
        window_id: keystone_core::WindowId,
        launch: bool,
        fixture: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer = cx.new(|cx| {
            TextInput::new("Write a draft…", InputMode::Multiline, cx)
                .text_style(TextSize::Body, Color::Strong)
        });
        let search = cx.new(|cx| TextInput::new("Search", InputMode::SingleLine, cx));
        let rename_input = cx.new(|cx| TextInput::new("Session title", InputMode::SingleLine, cx));
        let worktree_input =
            cx.new(|cx| TextInput::new("Worktree name", InputMode::SingleLine, cx));
        let mut subscriptions = vec![
            cx.subscribe(&composer, |this, _, _: &Changed, cx| {
                if let Some(session) = this.selected(cx) {
                    this.share_draft(session, cx);
                }
                this.draft_save_task = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(250))
                        .await;
                    // The sidebar shows each draft's first line, so it refreshes once the
                    // debounced save lands rather than on every keystroke.
                    let _ = this.update(cx, |this, cx| {
                        this.save_draft(cx);
                        this.refresh(cx);
                    });
                }));
                cx.notify();
            }),
            cx.subscribe(&search, |this, _, _: &Changed, cx| this.refresh(cx)),
            cx.observe_window_appearance(window, |this, window, cx| this.apply_theme(window, cx)),
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    let id = this.window_id;
                    this.state.update(cx, |state, _| state.notifier.used(id));
                    this.refresh_changes(cx);
                }
                cx.notify()
            }),
            cx.observe_window_bounds(window, |this, window, cx| {
                this.save_placement_soon(window, cx)
            }),
        ];
        let app = cx.weak_entity();
        let session_sidebar = cx.new(|_| SessionSidebar {
            app,
            hovered: None,
            shows_time: false,
        });
        let (wake_tx, mut wake_rx) = futures::channel::mpsc::unbounded::<()>();
        cx.spawn(async move |this, cx| {
            while wake_rx.next().await.is_some() {
                if this
                    .update(cx, |this, cx| {
                        if this.receive_customization() {
                            this.refresh(cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        // Relative times age, so the sidebar redraws once a minute while it shows one.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(60))
                    .await;
                let aged = this.update(cx, |this, cx| {
                    if this.presentation.current.sidebar && this.session_sidebar.read(cx).shows_time
                    {
                        this.refresh(cx);
                    } else if this.state.read(cx).has_usage() {
                        // Usage readings age and reset too.
                        cx.notify();
                    }
                });
                if aged.is_err() {
                    break;
                }
            }
        })
        .detach();
        let wake: Wake = Arc::new(move || {
            let _ = wake_tx.unbounded_send(());
        });
        // A window reopens on the project it last showed, even when its folder has gone,
        // so that project's sessions stay readable. At launch, KEYSTONE_PROJECT_DIR opens
        // the first window on that project instead.
        let launch_project = std::env::var_os("KEYSTONE_PROJECT_DIR")
            .filter(|_| launch && window_id == keystone_core::WindowId::FIRST);
        let saved_project = state
            .read(cx)
            .store()
            .workspace()
            .preferences()
            .window_project(window_id)
            .filter(|_| launch_project.is_none());
        // A window that never saved a project, as after an upgrade from before projects,
        // opens on the folder of the session it shows, so that session stays in its
        // sidebar, else on the folder Keystone was launched in.
        let selected_folder = {
            let workspace = state.read(cx).store().workspace();
            workspace
                .selected_in(window_id)
                .and_then(|id| workspace.sessions().iter().find(|s| s.id() == id))
                .and_then(|session| match session.context() {
                    keystone_core::SessionContext::Recorded { directory, .. } => {
                        Some(keystone_core::ProjectId::of(directory))
                    }
                    _ => None,
                })
                .filter(|_| launch_project.is_none())
        };
        let project = match saved_project.or(selected_folder) {
            Some(project) => Ok(project),
            None => {
                let project_dir = launch_project
                    .or_else(|| std::env::var_os("KEYSTONE_PROJECT_DIR"))
                    .map(PathBuf::from)
                    .or_else(|| std::env::current_dir().ok())
                    .unwrap_or_else(|| PathBuf::from("."));
                keystone_core::ProjectId::new(&project_dir).map_err(|e| e.to_string())
            }
        };
        let preferences = state.read(cx).store().workspace().preferences().clone();
        let presentation = Presentation::new(project, window_id, &preferences, cx);
        subscriptions.push(
            cx.subscribe(&state, |this, _, changed: &RuntimeChanged, cx| {
                this.runtime_changed(changed, cx);
            }),
        );
        let slash = slash_menu::SlashMenu::new(&composer, cx);
        let mentions = mentions::Mentions::new(&composer, cx);
        let mut result = Self {
            state,
            window_id,
            handle: window.window_handle(),
            context_menu: None,
            placement_task: None,
            titled: None,
            error: None,
            folder_warning: None,
            composer,
            search,
            session_sidebar,
            _subscriptions: subscriptions,
            draft_save_task: None,
            focus: cx.focus_handle(),
            recovery_focus: (
                cx.focus_handle().tab_stop(true),
                cx.focus_handle().tab_stop(true),
            ),
            new_menu: false,
            filter_menu: false,
            provider_filter: None,
            settings: None,
            approval_menu: false,
            model_menu: None,
            confirm_full_access: None,
            renaming: None,
            rename_input,
            worktree_dialog: None,
            worktree_input,
            window_drag: false,
            settled_open: false,
            fixture,
            presentation,
            customize_wake: wake,
            transcript_scroll: ScrollHandle::new(),
            replies: RefCell::default(),
            reveal_task: None,
            clock: cx.background_executor().clone(),
            fixture_document: Arc::new(Document::parse(if fixture {
                include_str!("fixture.md")
            } else {
                ""
            })),
            customize_agent: None,
            palette: None,
            projects: None,
            prompt_list: None,
            files: files::Files::default(),
            changes: changes::Changes::default(),
            notes: notes::Notes::default(),
            slash,
            mentions,
            attach: attachments::Attach::new(cx),
            usage_focus: cx.focus_handle().tab_stop(true),
        };
        result.save_opened_project(cx);
        result.sync_draft(cx);
        result.apply_theme(window, cx);
        result
    }

    fn apply_theme(&self, window: &Window, cx: &mut App) {
        keystone_ui::set_theme(
            settings::theme_for(
                self.state
                    .read(cx)
                    .store()
                    .workspace()
                    .preferences()
                    .appearance(),
                window,
                cx,
            ),
            cx,
        );
    }

    fn update_appearance(
        &mut self,
        change: impl FnOnce(&mut Appearance),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut appearance = self
            .state
            .read(cx)
            .store()
            .workspace()
            .preferences()
            .appearance()
            .clone();
        change(&mut appearance);
        self.error = match self
            .state
            .update(cx, |state, cx| state.set_appearance(appearance, cx))
        {
            Ok(applied) => durability_warning(applied.durability),
            Err(error) => Some(format!("Settings not saved. {error}")),
        };
        self.apply_theme(window, cx);
        self.refresh(cx);
    }

    /// Notifies this view and the cached sidebar. Use it for any change the sidebar shows.
    /// Composer typing and draft saves notify only this view, so they reuse the sidebar.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        cx.notify();
        self.session_sidebar.update(cx, |_, cx| cx.notify());
    }

    /// Shows the selected session's draft: as typed in another window that shows it,
    /// else as saved.
    fn sync_draft(&mut self, cx: &mut Context<Self>) {
        let selected = self.selected(cx);
        let this = cx.entity();
        let typed = open_windows(cx)
            .into_iter()
            .filter(|other| other != &this)
            .find(|other| selected.is_some() && other.read(cx).selected(cx) == selected)
            .map(|other| {
                let input = other.read(cx).composer.read(cx);
                (input.value().to_owned(), Some(input.tokens().to_vec()))
            });
        let (value, tokens) = typed.unwrap_or_else(|| {
            let draft = self
                .state
                .read(cx)
                .store()
                .workspace()
                .sessions()
                .iter()
                .find(|s| Some(s.id()) == selected)
                .map(|s| s.draft().to_owned())
                .unwrap_or_default();
            (draft, None)
        });
        self.mentions.edited = None;
        self.composer
            .update(cx, |input, cx| input.set_value(value, cx));
        match tokens {
            Some(tokens) => self
                .composer
                .update(cx, |input, cx| input.set_tokens(tokens, cx)),
            None => self.restore_mention_tokens(cx),
        }
    }

    /// Shows the draft just saved for `session` in every other window that shows it,
    /// so a session open in two windows has one draft.
    fn share_draft(&self, session: u64, cx: &mut Context<Self>) {
        let this = cx.entity();
        let text = self.composer.read(cx).value().to_owned();
        let tokens = self.composer.read(cx).tokens().to_vec();
        for other in open_windows(cx) {
            if other == this {
                continue;
            }
            other.update(cx, |other, cx| {
                let input = other.composer.read(cx);
                if other.selected(cx) == Some(session)
                    && (input.value() != text || input.tokens() != tokens)
                {
                    other.draft_save_task = None;
                    other.mentions.edited = None;
                    other.composer.update(cx, |input, cx| {
                        if input.value() != text {
                            input.set_value(text.clone(), cx);
                        }
                        input.set_tokens(tokens.clone(), cx);
                    });
                }
            });
        }
    }

    /// Saves this window's placement once it has stopped moving, so a crash keeps it.
    fn save_placement_soon(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.placement_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(500))
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let (id, placement) = (this.window_id, placement(window));
                let project = this.presentation.project.clone().ok();
                let _ = this.state.update(cx, |state, _| {
                    state.set_placement(id, placement, project.as_ref())
                });
            });
        }));
    }

    fn save_draft(&mut self, cx: &mut Context<Self>) -> bool {
        let session_id = self.selected(cx);
        let result = if let Some(id) = session_id {
            let draft = self.composer.read(cx).value().to_owned();
            self.state
                .update(cx, |state, cx| state.set_draft(id, draft, cx))
                .map(|applied| applied.durability)
        } else {
            self.state
                .update(cx, |state, cx| state.sync_pending(cx))
                .map(|()| Durability::Synced)
                .map_err(Into::into)
        };
        match result {
            Ok(durability) => {
                if let Some(id) = session_id {
                    self.share_draft(id, cx);
                }
                self.error = durability_warning(durability);
                cx.notify();
                self.error.is_none()
            }
            Err(error) => {
                self.error = Some(format!(
                    "Draft not saved. Stay in this session and retry after fixing storage. {error}"
                ));
                cx.notify();
                false
            }
        }
    }

    /// Returns false, with the error shown, when the draft or the session cannot be
    /// saved, so the app stays open. The logs are written either way.
    #[cfg(test)]
    fn save_before_exit(&mut self, cx: &mut Context<Self>) -> bool {
        self.draft_save_task = None;
        let drafts = self.composer_draft(cx).into_iter().collect();
        self.state
            .update(cx, |state, cx| {
                state.save(drafts, Vec::new(), Vec::new(), cx)
            })
            .is_ok()
    }

    /// Creates a session and selects it. Given a layout, tabs and a view, saves them
    /// with the view holding the new session, in the same write.
    fn create(
        &mut self,
        provider: Provider,
        show: Option<(
            keystone_core::Composition,
            keystone_core::SessionTabs,
            keystone_core::ViewId,
        )>,
        cx: &mut Context<Self>,
    ) {
        let directory = match self.presentation.creation_directory() {
            Ok(directory) => directory,
            Err(error) => {
                self.error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        let project = self.presentation.project.as_ref().ok().cloned();
        let show = show.and_then(|(layout, tabs, view)| {
            project
                .as_ref()
                .map(|project| (project, layout, tabs, view))
        });
        let window = self.window_id;
        self.create_with(cx, |state, cx| {
            state.create_showing(provider, &directory, now_ms(), window, show, cx)
        });
    }

    /// Saves the draft, then creates and selects the session `create` makes.
    fn create_with(
        &mut self,
        cx: &mut Context<Self>,
        create: impl FnOnce(
            &mut AppState,
            &mut Context<AppState>,
        ) -> Result<keystone_core::Applied<u64>, Box<dyn std::error::Error>>,
    ) {
        if !self.save_draft(cx) {
            return;
        }
        if let Some(left) = self.selected(cx) {
            let rows = self.replies.get_mut().settle(left);
            self.invalidate(left, ranges(rows), false, cx);
        }
        let previous = self.selected(cx);
        self.error = match self.state.update(cx, create) {
            Ok(applied) => durability_warning(applied.durability),
            Err(error) => Some(error.to_string()),
        };
        if self.selected(cx) != previous {
            self.presentation.selection_changed();
            self.approval_menu = false;
            self.confirm_full_access = None;
        }
        self.new_menu = false;
        self.model_menu = None;
        self.sync_draft(cx);
        self.refresh(cx);
    }

    fn toggle_settle(&mut self, cx: &mut Context<Self>) {
        let selected = self
            .state
            .read(cx)
            .store()
            .workspace()
            .sessions()
            .iter()
            .find(|s| Some(s.id()) == self.selected(cx))
            .map(|s| (s.id(), s.settled()));
        if let Some((id, settled)) = selected {
            self.settle(id, !settled, cx);
        }
    }

    /// Settles or un-settles one session. Selection does not change.
    fn settle(&mut self, id: u64, settled: bool, cx: &mut Context<Self>) {
        if settled
            && let Some(reason) = self
                .state
                .read(cx)
                .conversations()
                .get(&id)
                .and_then(Conversation::settle_blocker)
        {
            self.error = Some(reason.into());
            self.refresh(cx);
            return;
        }
        if !self.save_draft(cx) {
            return;
        }
        self.error = match self
            .state
            .update(cx, |state, cx| state.set_settled(id, settled, cx))
        {
            Ok(applied) => durability_warning(applied.durability),
            Err(error) => Some(error.to_string()),
        };
        self.refresh(cx);
    }

    fn transcript(&self) -> gpui::Div {
        v_flex()
            .gap_5()
            .text_size(TextSize::Body.px())
            .line_height(px(22.))
            .child(
                Label::new("Interface preview · sample transcript")
                    .size(TextSize::Ui)
                    .color(Color::Muted),
            )
            .child(Markdown::new("fixture", self.fixture_document.clone()))
    }

    /// Selects `id` and shows it where [`Keystone::place`] puts it, in one write.
    fn select(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some((view, change)) = self.place(id, cx) else {
            self.select_showing(id, None, cx);
            return;
        };
        if self.select_showing(id, change, cx) {
            self.show_fitted(view, cx);
            self.presentation.current.arrangement.seen = Some(Some(id));
        }
    }

    /// Selects `id`, saving the layout and tabs given in the same write. Returns whether
    /// it was saved.
    pub(crate) fn select_showing(
        &mut self,
        id: u64,
        shown: Option<session_tabs::Shown>,
        cx: &mut Context<Self>,
    ) -> bool {
        let previous = self.selected(cx);
        if !self.save_draft(cx) {
            return false;
        }
        if let Some(left) = self.selected(cx) {
            let rows = self.replies.get_mut().settle(left);
            self.invalidate(left, ranges(rows), false, cx);
        }
        let on_screen = self.session_tabs(cx).view_of(id).is_some_and(|view| {
            self.presentation
                .current
                .arrangement
                .visible
                .contains(&view)
        });
        if !on_screen {
            self.replies.get_mut().forget(id);
        }
        self.presentation.current.arrangement.sidebar_overlay = false;
        let project = self.presentation.project.clone();
        let window_id = self.window_id;
        let result = self.state.update(cx, |state, cx| match (shown, &project) {
            (None, _) => state.select(window_id, id, cx),
            (Some((layout, tabs)), Ok(project)) => {
                state.show(project, window_id, layout, tabs, Some(id), cx)
            }
            (Some(_), Err(error)) => {
                Err(format!("Cannot save this project's layout. {error}").into())
            }
        });
        let saved = result.is_ok();
        self.error = match result {
            Ok(applied) => durability_warning(applied.durability),
            Err(error) => Some(error.to_string()),
        };
        if self.selected(cx) != previous {
            self.presentation.selection_changed();
            self.approval_menu = false;
            self.model_menu = None;
            self.confirm_full_access = None;
        }
        self.sync_draft(cx);

        self.refresh(cx);
        saved
    }

    fn start(&mut self, provider: Provider, window: &mut Window, cx: &mut Context<Self>) {
        self.start_showing(provider, None, window, cx);
    }

    /// Starts a session as [`Keystone::start`] does, saving `show` with it as
    /// [`Keystone::create`] does.
    pub(crate) fn start_showing(
        &mut self,
        provider: Provider,
        show: Option<(
            keystone_core::Composition,
            keystone_core::SessionTabs,
            keystone_core::ViewId,
        )>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.create(provider, show, cx);
        if self.error.is_none() && self.selected(cx).is_some() {
            self.composer.focus_handle(cx).focus(window, cx);
        }
    }

    /// Lets a press on `bar` move the window, for window managers that draw no title
    /// bar. The press arms the move before any control sees it, and a control wrapped in
    /// [`Keystone::own_press`] disarms it, so a press that drifts on a control still clicks.
    fn drag_window(&self, bar: gpui::Div, cx: &mut Context<Self>) -> gpui::Div {
        bar.capture_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, _, _| {
            this.window_drag = event.button == MouseButton::Left;
        }))
        .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, _| {
            if std::mem::take(&mut this.window_drag) && event.dragging() {
                window.start_window_move();
            }
        }))
    }

    /// Keeps a press on `control` from moving the window around it.
    fn own_press(&self, control: impl IntoElement, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.window_drag = false),
            )
            .child(control)
    }

    fn toggle_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        if self.presentation.current.arrangement.fitted {
            self.presentation.current.arrangement.sidebar_overlay = self
                .arrangement_has_sidebar(cx)
                && !self.presentation.current.arrangement.sidebar_overlay;
        } else {
            self.presentation.current.sidebar = !self.presentation.current.sidebar;
        }
        self.refresh(cx);
    }

    /// The main column's bar: the sidebar toggle, the breadcrumb, and the window controls.
    fn top_bar(
        &self,
        session: Option<(String, bool, Option<(String, bool)>)>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let hover = cx.theme().colors.element_hover;
        let bar = h_flex()
            .flex_shrink_0()
            .h(cx.theme().density.control(px(44.)))
            .px(px(6.))
            .gap(px(10.))
            .child(
                self.own_press(
                    IconButton::new("sidebar-toggle", IconName::Panel)
                        .debug_selector(|| "sidebar-toggle".into())
                        .tooltip(Tooltip::text(if self.presentation.current.sidebar {
                            "Hide sidebar"
                        } else {
                            "Show sidebar"
                        }))
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(ToggleSidebar), cx)
                        }),
                    cx,
                ),
            )
            .child(
                Label::new(
                    self.presentation
                        .project
                        .as_ref()
                        .map_or("keystone", |project| project.name())
                        .to_owned(),
                )
                .color(Color::Muted),
            )
            .when(self.settings.is_some(), |d| {
                d.child(Label::new("/").color(Color::Faint)).child(
                    Label::new("Settings")
                        .weight(FontWeight::MEDIUM)
                        .color(Color::Strong),
                )
            })
            .when_some(session, |d, (title, settled, branch)| {
                d.child(Label::new("/").color(Color::Faint))
                    .child(
                        div()
                            .id("rename-session")
                            .automation_id("rename-session")
                            .min_w_0()
                            .px(px(6.))
                            .py(px(2.))
                            .rounded(px(6.))
                            .cursor_pointer()
                            .hover(move |s| s.bg(hover))
                            .on_mouse_down(
                                MouseButton::Left,
                                self.scoped_listener(cx, |this, _, _, _| this.window_drag = false),
                            )
                            .child(
                                Label::new(title)
                                    .weight(FontWeight::MEDIUM)
                                    .color(Color::Strong)
                                    .truncate(),
                            )
                            .tooltip(Tooltip::with_key("Rename session", "F2").builder())
                            .on_click(self.scoped_listener(cx, |this, _, window, cx| {
                                this.open_rename(window, cx)
                            })),
                    )
                    .when(settled, |d| {
                        d.child(Label::new("Settled").color(Color::Muted))
                    })
                    .children(
                        branch.map(|(branch, missing)| worktrees::branch_chip(branch, missing)),
                    )
                    .children(self.pull_request_link(cx))
                    .child(self.own_press(handoff::button(), cx))
            })
            .child(div().flex_1())
            .when(self.settings.is_none(), |d| {
                d.children(self.split_toggle(cx))
                    .child(self.customize_toggle(cx))
            })
            .child(
                self.own_press(
                    h_flex()
                        .gap(px(10.))
                        .child(
                            IconButton::new("minimize", IconName::Minus)
                                .debug_selector(|| "minimize".into())
                                .tooltip(Tooltip::text("Minimize"))
                                .on_click(|_, window, _| window.minimize_window()),
                        )
                        .child(
                            IconButton::new("maximize", IconName::Square)
                                .debug_selector(|| "maximize".into())
                                .tooltip(Tooltip::text("Maximize"))
                                .on_click(|_, window, _| window.zoom_window()),
                        )
                        .child(
                            IconButton::new("close", IconName::Close)
                                .debug_selector(|| "close".into())
                                .tooltip(Tooltip::with_key("Close window", "Ctrl+Shift+W"))
                                .on_click({
                                    let root = cx.weak_entity();
                                    move |_, window, cx| {
                                        if close_window(&root, window, cx) {
                                            window.remove_window();
                                        }
                                    }
                                }),
                        ),
                    cx,
                ),
            );
        self.drag_window(bar, cx)
    }

    /// Opens the rename dialog for the selected session, filled with its title.
    fn open_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.selected_session(cx) else {
            return;
        };
        let (id, title) = (session.id(), session.title().to_owned());
        let field = self.rename_input.focus_handle(cx);
        self.renaming = Some(Renaming {
            id,
            focus: DialogFocus::with_body(field, window, cx),
            error: None,
        });
        self.rename_input.update(cx, |input, cx| {
            input.set_value(title, cx);
            input.select_all(&SelectAll, window, cx);
        });
        cx.notify();
    }

    fn close_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.renaming = None;
        self.composer.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    /// Saves the typed title. A blank one keeps the dialog open; a failed save keeps
    /// it open with the typed title and the error, so the user can retry.
    fn confirm_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.renaming.as_ref().map(|renaming| renaming.id) else {
            return;
        };
        let title = self.rename_input.read(cx).value().to_owned();
        match self
            .state
            .update(cx, |state, cx| state.rename(id, &title, cx))
        {
            Ok(applied) => {
                self.error = durability_warning(applied.durability);
                self.close_rename(window, cx);
            }
            Err(error) => {
                if let Some(renaming) = &mut self.renaming {
                    renaming.error = Some(format!("Title not saved. {error}"));
                }
                self.rename_input.focus_handle(cx).focus(window, cx);
            }
        }
        self.refresh(cx);
    }

    fn rename_dialog(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let renaming = self.renaming.as_ref()?;
        let colors = cx.theme().colors;
        let cancel = cx.listener(|this, _: &(), window, cx| this.close_rename(window, cx));
        let confirm =
            Rc::new(cx.listener(|this, _: &(), window, cx| this.confirm_rename(window, cx)));
        let submit = confirm.clone();
        let field = div()
            .px(px(10.))
            .rounded(px(6.))
            .border_1()
            .border_color(colors.border_focused)
            .bg(colors.background)
            .automation_id("rename-field")
            .on_key_down(move |event: &gpui::KeyDownEvent, window, cx| {
                if event.keystroke.key == "enter" {
                    submit(&(), window, cx);
                    cx.stop_propagation();
                }
            })
            .child(self.rename_input.clone());
        let body = v_flex()
            .gap(spacing::SM)
            .child(field)
            .children(renaming.error.clone().map(|error| {
                div()
                    .automation_id("rename-error")
                    .child(Label::new(error).size(TextSize::Sm).color(Color::Danger))
            }));
        Some(
            ConfirmDialog::new(
                "rename-dialog",
                &renaming.focus,
                "Rename session",
                "Shown in the sidebar and above the conversation.",
            )
            .icon(IconName::Pencil)
            .tone(Color::Accent)
            .confirm_label("Rename")
            .body(body)
            .on_cancel(move |window, cx| cancel(&(), window, cx))
            .on_confirm(move |window, cx| confirm(&(), window, cx)),
        )
    }

    fn sidebar_column(&self, width: f32, side: SidebarSide, cx: &mut Context<Self>) -> gpui::Div {
        let (colors, density) = (cx.theme().colors, cx.theme().density);
        let list = AnyView::from(self.session_sidebar.clone());
        v_flex()
            .relative()
            .w(px(width))
            .flex_shrink_0()
            .h_full()
            .overflow_hidden()
            .bg(colors.sidebar)
            .capture_any_mouse_down(
                self.scoped_listener(cx, |this, _, window, cx| this.focus.focus(window, cx)),
            )
            .map(|d| match side {
                SidebarSide::Left => d.hairline_r(cx),
                SidebarSide::Right => d.hairline_l(cx),
            })
            .child(
                self.drag_window(
                    h_flex()
                        .flex_shrink_0()
                        .h(density.control(px(44.)))
                        .pl(px(13.))
                        .pr_2()
                        .gap(px(10.))
                        .child(arch_mark(px(20.)))
                        .child(
                            Label::new("keystone")
                                .size(TextSize::Body)
                                .weight(FontWeight::SEMIBOLD),
                        )
                        .child(
                            h_flex()
                                .h(px(18.))
                                .px(px(6.))
                                .rounded(px(5.))
                                .bg(colors.element_hover)
                                .child(
                                    Label::new("preview").size(TextSize::Xs).color(Color::Muted),
                                ),
                        )
                        .child(div().flex_1()),
                    cx,
                ),
            )
            .child(self.project_switcher(cx))
            .child(
                h_flex()
                    .flex_shrink_0()
                    .h(density.control(px(36.)))
                    .pr_2()
                    .child(
                        h_flex()
                            .id("search")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .pl(px(14.))
                            .gap_2()
                            .cursor(gpui::CursorStyle::IBeam)
                            .on_click(self.scoped_listener(cx, |this, _, window, cx| {
                                this.search.focus_handle(cx).focus(window, cx)
                            }))
                            .child(Icon::new(IconName::Search).size(px(15.)))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .automation_id("search-field")
                                    .child(self.search.clone()),
                            ),
                    )
                    .child(self.filter_menu(cx))
                    .child(self.new_session_menu(cx)),
            )
            // A reused cached view skips prepaint, so it would report no element
            // geometry. Verification trades that speed for names.
            .map(|d| {
                if automation::enabled() {
                    d.child(div().flex_1().min_h_0().w_full().child(list))
                } else {
                    d.child(list.cached(StyleRefinement::default().flex_1().min_h_0().w_full()))
                }
            })
            .child(self.sidebar_footer(cx))
    }

    /// The bottom of the sidebar and of the settings page's column. Its cog shows
    /// selected while the page is open and toggles it.
    fn sidebar_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .flex_shrink_0()
            .h(cx.theme().density.control(px(40.)))
            .pl(px(6.))
            .pr(px(14.))
            .gap_0p5()
            .hairline_t(cx)
            .child(
                IconButton::new("settings", IconName::Settings)
                    .size(ButtonSize::Large)
                    .debug_selector(|| "settings".into())
                    .selected(self.settings.is_some())
                    .tooltip(Tooltip::text("Settings"))
                    .on_click(self.scoped_listener(cx, |this, _, window, cx| {
                        this.toggle_settings(window, cx)
                    })),
            )
            .child(
                Label::new("Local workspace")
                    .size(TextSize::Ui)
                    .color(Color::Muted),
            )
    }

    fn filter_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let choices = [
            (None, "All sessions", "filter-all"),
            (Some(Provider::Claude), "Claude Code", "filter-claude"),
            (Some(Provider::Codex), "Codex", "filter-codex"),
        ];
        let mut menu = PopoverMenu::new(
            "filter-menu",
            IconButton::new("filter", IconName::Filter)
                .debug_selector(|| "filter".into())
                .selected(self.filter_menu || self.provider_filter.is_some())
                .tooltip(Tooltip::text("Filter sessions")),
        )
        .anchor(Anchor::TopRight)
        .open(self.filter_menu)
        .on_open_change(self.scoped_listener(cx, |this, open: &bool, _, cx| {
            this.filter_menu = *open;
            cx.notify();
        }));
        for (provider, label, id) in choices {
            menu = menu.child(
                MenuItem::new(id, label)
                    .checked(self.provider_filter == provider)
                    .debug_selector(|| id.into())
                    .on_click(self.scoped_listener(cx, move |this, _, _, cx| {
                        this.provider_filter = provider;
                        this.filter_menu = false;
                        this.refresh(cx);
                    })),
            );
        }
        menu
    }

    fn new_session_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        PopoverMenu::new(
            "new-session-menu",
            IconButton::new("new-session", IconName::Pencil)
                .debug_selector(|| "new-session".into())
                .selected(self.new_menu)
                .tooltip(Tooltip::text("New session")),
        )
        .anchor(Anchor::TopRight)
        .open(self.new_menu)
        .on_open_change(self.scoped_listener(cx, |this, open: &bool, _, cx| {
            this.new_menu = *open;
            cx.notify();
        }))
        .child(
            MenuItem::new("new-claude", "New Claude session")
                .icon(IconName::Chat)
                .kbd("Ctrl+N")
                .debug_selector(|| "new-claude".into())
                .on_click(self.scoped_listener(cx, |this, _, window, cx| {
                    this.start(Provider::Claude, window, cx)
                })),
        )
        .child(
            MenuItem::new("new-codex", "New Codex session")
                .icon(IconName::Chat)
                .kbd("Ctrl+Shift+N")
                .debug_selector(|| "new-codex".into())
                .on_click(self.scoped_listener(cx, |this, _, window, cx| {
                    this.start(Provider::Codex, window, cx)
                })),
        )
        .child(
            MenuItem::new("new-worktree", "New session in worktree…")
                .icon(IconName::Branch)
                .kbd("Ctrl+Alt+G")
                .debug_selector(|| "new-worktree".into())
                .on_click(
                    self.scoped_listener(cx, |this, _, window, cx| {
                        this.open_new_worktree(window, cx)
                    }),
                ),
        )
    }

    /// The welcome screen renders no composer, so focusing it there would strand the
    /// window's shortcuts; the root keeps them working.
    fn focus_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected(cx).is_some() {
            self.composer.focus_handle(cx).focus(window, cx);
        } else {
            self.focus.focus(window, cx);
        }
    }

    fn composer(
        &self,
        current: Option<(u64, Provider)>,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let colors = cx.theme().colors;
        let conversation = current.and_then(|(id, _)| self.state.read(cx).conversations().get(&id));
        let running = conversation.is_some_and(|c| c.running);
        let waiting = conversation.is_some_and(|c| !c.approvals.is_empty());
        let mode = self.selected_session(cx).map(|s| s.approval_mode());
        let can_send = current.is_some_and(|(id, _)| !self.importing(id))
            && (!self.composer.read(cx).value().trim().is_empty()
                || !self.draft_attachments(cx).is_empty());
        let field = div()
            .id("composer-field")
            .automation_id("composer-field")
            .min_h(px(44.))
            .pr(px(6.));
        let (field, toolbar) = match current {
            None => (
                field
                    .text_size(TextSize::Body.px())
                    .line_height(px(20.))
                    .text_color(colors.text_placeholder)
                    .child("Choose a provider below to start a session…"),
                h_flex()
                    .gap_2()
                    .child(self.welcome_pill(Provider::Claude, cx))
                    .child(self.welcome_pill(Provider::Codex, cx))
                    .child(self.worktree_pill(cx))
                    .child(div().flex_1())
                    .child(
                        Label::new("Ctrl N · Ctrl Shift N")
                            .size(TextSize::Sm)
                            .color(Color::Muted),
                    ),
            ),
            Some((session, provider)) => {
                let status = if waiting {
                    "Waiting for your approval".to_owned()
                } else if running {
                    "Ctrl+Enter steers this turn".to_owned()
                } else if let Some(hint) = self.slash_hint(session, provider, cx) {
                    format!("{hint} · Ctrl+Enter to send")
                } else {
                    "Ctrl+Enter to send · Enter for a new line".to_owned()
                };
                let id = session;
                let folder = self.conversation_folder(id, cx).unwrap();
                let action = folder.clone();
                let project = div()
                    .id("conversation-folder")
                    .flex_none()
                    .tooltip(Tooltip::text(folder.tooltip()).builder())
                    .child(
                        folder
                            .button("project")
                            .aria_label(folder.tooltip())
                            .track_focus(&self.presentation.current.folder_focus)
                            .debug_selector(|| "project".into())
                            .on_click(self.scoped_listener(cx, move |this, _, _, cx| {
                                if this.selected(cx) != Some(id) {
                                    return;
                                }
                                match &action {
                                    presentation::ConversationFolder::Recorded(directory) => {
                                        cx.open_with_system(directory.path())
                                    }
                                    presentation::ConversationFolder::LegacyMissing => {
                                        this.choose_legacy_folder(id, cx)
                                    }
                                    presentation::ConversationFolder::Unsupported => {}
                                }
                            })),
                    );
                let narrow = self
                    .presentation
                    .current
                    .arrangement
                    .narrows_composer(viewport.width);
                let toolbar = h_flex()
                    .gap(px(14.))
                    .child(
                        h_flex()
                            .flex_shrink(1.)
                            .min_w(model_selector::MIN_WIDTH)
                            .gap(px(7.))
                            .child(self.model_selector(session, provider, viewport, cx))
                            .when(!narrow, |d| {
                                d.child(
                                    div()
                                        .id("composer-hint")
                                        .automation_id("composer-hint")
                                        .flex_shrink(1000.)
                                        .min_w_0()
                                        .child(
                                            Label::new(status)
                                                .size(TextSize::Ui)
                                                .color(Color::Muted)
                                                .truncate(),
                                        ),
                                )
                            }),
                    )
                    .child(div().flex_shrink_0().w_px().h(px(14.)).bg(colors.border))
                    .child(project)
                    .when_some(mode, |d, mode| {
                        d.child(self.approval_menu(mode, running, narrow, cx))
                    })
                    .children(self.usage_meter(session, narrow, cx))
                    .child(div().flex_1())
                    .child(
                        h_flex()
                            .flex_none()
                            .gap(px(6.))
                            .child(
                                Button::new("attach", "")
                                    .style(ButtonStyle::Ghost)
                                    .icon(IconName::Paperclip)
                                    .aria_label("Attach files")
                                    .debug_selector(|| "attach".into())
                                    .tooltip(Tooltip::with_key("Attach files", "Ctrl+Shift+U"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.choose_attachments(window, cx)
                                    })),
                            )
                            .when(running, |d| {
                                d.child(
                                    Button::new("stop", "Stop")
                                        .style(ButtonStyle::Subtle)
                                        .icon(IconName::Square)
                                        .debug_selector(|| "stop".into())
                                        .tooltip(Tooltip::text("Stop this turn"))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.stop_turn(session, window, cx)
                                        })),
                                )
                            })
                            .child(
                                div()
                                    .id("send")
                                    .automation_id("send")
                                    .flex_shrink_0()
                                    .size(px(32.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_full()
                                    .bg(if can_send {
                                        colors.element_inverse
                                    } else {
                                        colors.element_disabled
                                    })
                                    .child(Icon::new(IconName::Up).size(px(15.)).color(
                                        if can_send {
                                            Color::Custom(colors.text_inverse)
                                        } else {
                                            Color::Faint
                                        },
                                    ))
                                    .tooltip(
                                        Tooltip::with_key(
                                            if running { "Steer" } else { "Send" },
                                            "Ctrl+Enter",
                                        )
                                        .builder(),
                                    )
                                    .when(can_send, |d| {
                                        d.cursor_pointer().hover(|s| s.opacity(0.85)).on_click(
                                            cx.listener(move |this, _, window, cx| {
                                                if let Some(draft) = this
                                                    .composer_draft(cx)
                                                    .filter(|draft| draft.session == session)
                                                {
                                                    this.send_message(draft, window, cx)
                                                }
                                            }),
                                        )
                                    })
                                    .when(!can_send, |d| d.cursor_not_allowed()),
                            ),
                    );
                let field = if self
                    .slash_entries(cx)
                    .is_some_and(|(.., entries)| !entries.is_empty())
                {
                    self.slash_keys(field, cx)
                } else if self.mention_open(cx) {
                    let list = self.mentions.list.clone();
                    self.menu_keys(field, list, Self::choose_highlighted_mention, cx)
                } else {
                    field
                };
                (
                    field
                        .capture_action(cx.listener(
                            |this, _: &keystone_ui::text_input::Paste, window, cx| {
                                if this.paste_image(window, cx) {
                                    cx.stop_propagation();
                                }
                            },
                        ))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.composer.focus_handle(cx).focus(window, cx)
                        }))
                        .child(self.composer.clone()),
                    toolbar,
                )
            }
        };
        v_flex()
            // Not `w_full().max_w(..)`: taffy sizes the field from a pass that measures the
            // draft at the uncapped width, so a wrapped draft overflowed a one-row field.
            .w(px(CONTENT_WIDTH))
            .max_w_full()
            .flex_shrink_0()
            .bg(colors.composer)
            .border_1()
            .border_color(colors.composer_border)
            .rounded(radius::COMPOSER)
            .shadow(vec![BoxShadow {
                color: colors.composer_shadow,
                offset: point(px(0.), px(10.)),
                blur_radius: px(30.),
                spread_radius: px(0.),
                inset: false,
            }])
            .pt(px(16.))
            .pl(px(18.))
            .pr(px(12.))
            .pb(px(12.))
            .gap(px(14.))
            .relative()
            .when(current.is_some(), |d| {
                d.drag_over::<ExternalPaths>(move |style, _, _, _| {
                    style.border_color(colors.border_focused)
                })
                .on_drop(cx.listener(
                    |this, paths: &ExternalPaths, window, cx| this.on_file_drop(paths, window, cx),
                ))
            })
            .children(current.and_then(|_| self.slash_menu(viewport.width, cx)))
            .children(current.and_then(|_| self.mention_menu(viewport.width, cx)))
            .child(field)
            .children(current.and_then(|(session, _)| self.attachment_strip(session, cx)))
            .child(toolbar)
    }

    fn welcome_pill(&self, provider: Provider, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors;
        let (id, tooltip, key) = match provider {
            Provider::Claude => ("welcome-claude", "New Claude session", "Ctrl+N"),
            Provider::Codex => ("welcome-codex", "New Codex session", "Ctrl+Shift+N"),
        };
        h_flex()
            .id(id)
            .automation_id(id)
            .h(px(30.))
            .px(px(11.))
            .gap(px(7.))
            .rounded(px(15.))
            .bg(colors.element_filled)
            .cursor_pointer()
            .hover(move |s| s.bg(colors.element_filled_hover))
            .child(
                Icon::new(provider_icon(provider))
                    .size(px(14.))
                    .color(provider_icon_color(provider, cx)),
            )
            .child(
                Label::new(provider_name(provider))
                    .size(TextSize::Ui)
                    .weight(FontWeight::MEDIUM),
            )
            .tooltip(Tooltip::with_key(tooltip, key).builder())
            .on_click(self.scoped_listener(cx, move |this, _, window, cx| {
                this.start(provider, window, cx)
            }))
    }

    /// The session's provider and model as one button, with the menu that chooses
    /// them while the session has not sent.
    fn model_selector(
        &self,
        session: u64,
        provider: Provider,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selection = self.model_selection(session, provider, cx);
        let handlers = model_selector::Handlers {
            open_change: Rc::new(cx.listener(move |this, open: &bool, _, cx| {
                this.model_menu = open.then_some(session);
                if *open {
                    this.request_catalog(session, provider, cx);
                }
                cx.notify();
            })),
            choose_provider: Rc::new(cx.listener(move |this, provider: &Provider, window, cx| {
                this.choose_provider(session, *provider, window, cx)
            })),
            choose_account: Rc::new(cx.listener(
                move |this, (provider, account): &(Provider, Option<AccountId>), window, cx| {
                    this.choose_account(session, *provider, account.clone(), window, cx)
                },
            )),
            choose_model: Rc::new(cx.listener(
                move |this, (provider, model): &(Provider, Option<String>), window, cx| {
                    this.choose_model(session, *provider, model.clone(), window, cx)
                },
            )),
            retry: Rc::new(cx.listener(move |this, provider: &Provider, _, cx| {
                this.retry_catalog(session, *provider, cx)
            })),
        };
        let open = self.model_menu == Some(session);
        model_selector::render(selection, open, handlers, viewport, cx)
    }

    /// A provider chosen in the model menu. The session's model returns to the
    /// provider's default and the menu stays open on that provider's catalog. The
    /// current provider just closes the menu.
    fn choose_provider(
        &mut self,
        session: u64,
        provider: Provider,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self.session(session, cx).filter(|s| !s.started()) else {
            return self.close_model_menu(window, cx);
        };
        if current.provider() == provider {
            return self.close_model_menu(window, cx);
        }
        match self
            .state
            .update(cx, |state, cx| state.set_provider(session, provider, cx))
        {
            Ok(applied) => {
                self.error = durability_warning(applied.durability);
                self.request_catalog(session, provider, cx);
            }
            Err(error) => self.error = Some(format!("Provider not changed. {error}")),
        }
        // The sidebar shows the new placeholder title.
        self.refresh(cx);
    }

    /// An account chosen in the model menu, on the session's provider or another one.
    /// The menu stays open on the account's model catalog, since accounts may offer
    /// different models; choosing the current account just closes it.
    fn choose_account(
        &mut self,
        session: u64,
        provider: Provider,
        account: Option<AccountId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self.session(session, cx).filter(|s| !s.started()) else {
            return self.close_model_menu(window, cx);
        };
        if current.provider() == provider && current.account() == account.as_ref() {
            return self.close_model_menu(window, cx);
        }
        let result = self.state.update(cx, |state, cx| {
            state.set_provider_account(session, provider, account, cx)
        });
        match result {
            Ok(applied) => {
                self.error = durability_warning(applied.durability);
                self.request_catalog(session, provider, cx);
            }
            Err(error) => self.error = Some(format!("Account not changed. {error}")),
        }
        self.refresh(cx);
    }

    /// A model chosen in the model menu, saved for the session's first Send. The row
    /// was rendered for `provider`'s catalog: a choice from a catalog the session has
    /// left since is dropped.
    fn choose_model(
        &mut self,
        session: u64,
        provider: Provider,
        model: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(current) = self
            .session(session, cx)
            .filter(|s| !s.started() && s.provider() == provider)
            && current.model() != model.as_deref()
        {
            self.error = match self
                .state
                .update(cx, |state, cx| state.set_model(session, model, cx))
            {
                Ok(applied) => durability_warning(applied.durability),
                Err(error) => Some(format!("Model not saved. {error}")),
            };
        }
        self.close_model_menu(window, cx);
    }

    fn close_model_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.model_menu = None;
        self.composer.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    /// The session's approval mode, as a menu of every mode with what it allows. A
    /// `narrow` composer shows only the mode's icon, and names the mode in its tooltip.
    fn approval_menu(
        &self,
        mode: ApprovalMode,
        running: bool,
        narrow: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let session = self.selected(cx);
        let applies = if running {
            "a change applies from the next turn"
        } else {
            "applies from the next turn"
        };
        let color = (mode == ApprovalMode::FullAccess).then_some(Color::Warning);
        let trigger = if narrow {
            IconButton::new("approval-mode", IconName::Shield)
                .size(ButtonSize::Compact)
                .selected(self.approval_menu)
                .when_some(color, IconButton::color)
                .aria_label(format!("Approval mode · {}", mode.label()))
                .debug_selector(|| "approval-mode".into())
                .tooltip(Tooltip::text(format!(
                    "Approval mode · {} · {applies}",
                    mode.label()
                )))
                .into_any_element()
        } else {
            Button::new("approval-mode", mode.label())
                .size(ButtonSize::Compact)
                .icon(IconName::Shield)
                .selected(self.approval_menu)
                .when_some(color, Button::color)
                .debug_selector(|| "approval-mode".into())
                .tooltip(Tooltip::text(format!("Approval mode · {applies}")))
                .into_any_element()
        };
        PopoverMenu::new("approval-menu", trigger)
            .anchor(Anchor::BottomLeft)
            .open(self.approval_menu)
            .on_open_change(self.scoped_listener(cx, |this, open: &bool, _, cx| {
                this.approval_menu = *open;
                cx.notify();
            }))
            .child(MenuHeader::new("Approvals"))
            .children(ApprovalMode::ALL.map(|choice| {
                MenuItem::new(("approval", choice as usize), choice.label())
                    .detail(choice.description())
                    .checked(choice == mode)
                    .debug_selector(|| format!("approval-{}", choice.slug()))
                    .on_click(self.scoped_listener(cx, move |this, _, window, cx| {
                        if this.selected(cx) != session {
                            return;
                        }
                        this.choose_approval_mode(choice, window, cx)
                    }))
            }))
    }

    /// A choice from the approval menu. The current mode changes nothing, and Full
    /// access asks first; any other mode is saved and applied at once.
    fn choose_approval_mode(
        &mut self,
        mode: ApprovalMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.approval_menu = false;
        let Some(session) = self.selected_session(cx) else {
            return;
        };
        let (id, current) = (session.id(), session.approval_mode());
        if mode == current {
            self.composer.focus_handle(cx).focus(window, cx);
        } else if mode == ApprovalMode::FullAccess {
            self.confirm_full_access = Some(FullAccessConfirmation {
                session: id,
                activation: self.presentation.activation(),
                identity: Rc::new(()),
                focus: DialogFocus::new(window, cx),
            });
        } else {
            self.set_approval_mode(id, mode, window, cx);
        }
        cx.notify();
    }

    /// The confirmation for Full access, while it is open.
    fn full_access_dialog(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let confirmation = self.confirm_full_access.as_ref()?;
        let provider = provider_name(self.provider(confirmation.session, cx)?);
        let focus = &confirmation.focus;
        let cancel_identity = confirmation.identity.clone();
        let confirm_identity = confirmation.identity.clone();
        let cancel = self.scoped_listener(cx, move |this, _: &(), window, cx| {
            if this
                .take_full_access_confirmation(&cancel_identity, cx)
                .is_none()
            {
                return;
            }
            this.composer.focus_handle(cx).focus(window, cx);
            cx.notify();
        });
        let confirm = self.scoped_listener(cx, move |this, _: &(), window, cx| {
            if let Some(id) = this.take_full_access_confirmation(&confirm_identity, cx) {
                this.set_approval_mode(id, ApprovalMode::FullAccess, window, cx);
            }
        });
        Some(
            ConfirmDialog::new(
                "full-access-dialog",
                focus,
                "Turn on Full access?",
                format!(
                    "{provider} will run commands and edit files without asking, \
                     until you choose another approval mode."
                ),
            )
            .tone(Color::Warning)
            .on_cancel(move |window, cx| cancel(&(), window, cx))
            .on_confirm(move |window, cx| confirm(&(), window, cx)),
        )
    }

    fn take_full_access_confirmation(&mut self, identity: &Rc<()>, cx: &App) -> Option<u64> {
        let confirmation = self.confirm_full_access.as_ref()?;
        if !Rc::ptr_eq(identity, &confirmation.identity)
            || !self.presentation.matches(confirmation.activation)
            || self.selected(cx) != Some(confirmation.session)
        {
            return None;
        }
        Some(self.confirm_full_access.take().unwrap().session)
    }

    /// Saves the session's mode, then hands it to its provider at once, so a
    /// message already waiting to start a turn, or a steer that becomes the next turn,
    /// runs under it. The save comes first: it also records that the provider may hold
    /// an override, and nothing reaches the provider unless that is saved.
    fn set_approval_mode(
        &mut self,
        id: u64,
        mode: ApprovalMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self
            .state
            .update(cx, |state, cx| state.set_approval_mode(id, mode, cx))
        {
            Ok(applied) => {
                self.error = durability_warning(applied.durability);
            }
            Err(error) => self.error = Some(format!("Approval mode not saved. {error}")),
        }
        self.composer.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn invalidate(&mut self, session: u64, changed: Vec<Range<usize>>, status: bool, cx: &App) {
        if let Some(conversation) = self.state.read(cx).conversations().get(&session) {
            self.presentation
                .invalidate(session, conversation.rows.len(), &changed, status);
        }
    }

    fn reveal(&mut self, cx: &mut Context<Self>) {
        if self.reveal_task.is_some() || !self.replies.get_mut().pacing() {
            return;
        }
        self.reveal_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(reveal::TICK).await;
            let _ = this.update(cx, |this, cx| {
                this.reveal_task = None;
                let conversations = &this.state.read(cx).conversations();
                let changed = this
                    .replies
                    .get_mut()
                    .tick(this.clock.now(), |(session, index)| {
                        match conversations.get(&session)?.rows.get(index)? {
                            Row::Item(item) if item.kind == ItemKind::AgentMessage => Some(item),
                            _ => None,
                        }
                    });
                let mut sessions: HashMap<u64, Vec<usize>> = HashMap::new();
                for (session, index) in changed {
                    sessions.entry(session).or_default().push(index);
                }
                for (session, rows) in sessions {
                    this.invalidate(session, ranges(rows), false, cx);
                    cx.notify();
                }
                this.reveal(cx);
            });
        }));
    }

    /// Runs before the transcript list lays out. When the row the user is scrolled
    /// into was remeasured, the kept offset can now fall past its end, such as when a
    /// long reasoning summary is replaced by an empty one, or when the transcript
    /// widens and its text wraps onto fewer lines. The view would then skip the rows
    /// below it, so it moves to the first row after the place it was. It spans the
    /// transcript's width so it can measure the row at the width the list is about to
    /// use.
    fn transcript_top_check(&self, key: ViewKey, cx: &mut Context<Self>) -> impl IntoElement {
        let activation = self.presentation.activation();
        let app = cx.weak_entity();
        canvas(
            move |bounds, window, cx| {
                let _ = app.update(cx, |this, cx| {
                    if !this.presentation.matches(activation) {
                        return;
                    }
                    this.clamp_top(key, bounds.size.width, window, cx)
                });
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .w_full()
        .h_0()
    }

    fn clamp_top(
        &mut self,
        key: ViewKey,
        width: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let remeasured = self.presentation.current.views.take_remeasured_top(key);
        let Some(list) = self.presentation.current.views.get(key).cloned() else {
            return;
        };
        // The list remeasures every entry when its width changes, and it has not
        // laid out at this width yet.
        let resized = list.viewport_bounds().size.width != width;
        if !(remeasured || resized) || width <= px(0.) {
            return;
        }
        let space = size(AvailableSpace::Definite(width), AvailableSpace::MinContent);
        let mut top = list.logical_scroll_top();
        let start = top.item_ix;
        while top.item_ix < list.item_count() {
            let height = self
                .transcript_entry(key, top.item_ix, window, cx)
                .layout_as_root(space, window, cx)
                .height;
            if top.offset_in_item < height {
                break;
            }
            // The place was past this entry's end, or it has nothing to show.
            top = ListOffset {
                item_ix: top.item_ix + 1,
                offset_in_item: px(0.),
            };
        }
        if top.item_ix != start {
            list.scroll_to(top);
        }
    }

    fn composer_draft(&self, cx: &App) -> Option<ComposerDraft> {
        Some(ComposerDraft {
            session: self.selected(cx)?,
            text: self.composer.read(cx).value().to_owned(),
            attachments: self.draft_attachments(cx),
        })
    }

    /// The session's transcript: a virtualized list, so a long session lays out and
    /// paints only the rows in view. It sticks to the bottom while new rows arrive,
    /// unless the user has scrolled up.
    fn transcript_list(&mut self, key: ViewKey, cx: &mut Context<Self>) -> gpui::List {
        let rows = self
            .state
            .read(cx)
            .conversations()
            .get(&key.session)
            .map_or(0, |c| c.rows.len());
        let state = self.presentation.current.views.list(key, rows);
        self.presentation.current.views.begin_frame(key);
        let app = cx.weak_entity();
        let app_scroll = app.clone();
        state.set_scroll_handler(move |_, window, cx| {
            let _ = app_scroll.update(cx, |this, cx| {
                let keyboard = this.presentation.current.views.keyboard(key, cx);
                if keyboard.action == Some(view_state::CopyTarget::TextSelection)
                    && keyboard.action_focus.is_focused(window)
                    && window.context_stack().iter().any(|context| {
                        context.contains("MarkdownText") && context.contains("selection")
                    })
                {
                    return;
                }
                keyboard.row = None;
                keyboard.action = None;
                keyboard.selection_from_pointer = false;
                if keyboard.action_focus.is_focused(window) {
                    keyboard.action = None;
                    this.presentation
                        .current
                        .views
                        .focus(key, cx)
                        .focus(window, cx);
                    cx.notify();
                }
            });
        });
        list(state, move |index, window, cx| {
            app.update(cx, |this, cx| this.transcript_entry(key, index, window, cx))
                .unwrap_or_else(|_| div().into_any_element())
        })
        .size_full()
        .py_6()
    }

    fn jump_to_latest(&mut self, key: ViewKey, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let focus = self
            .presentation
            .current
            .views
            .keyboard(key, cx)
            .jump_focus
            .clone();
        self.presentation.current.views.scrolled_up(key).then(|| {
            h_flex()
                .absolute()
                .bottom_3()
                .left_0()
                .w_full()
                .justify_center()
                .child(gpui::deferred(
                    div().occlude().child(
                        Button::new(
                            arrangement::selector("jump-to-latest", key.view),
                            "Jump to latest",
                        )
                        .style(ButtonStyle::Filled)
                        .track_focus(&focus)
                        .icon(IconName::Chevron)
                        .debug_selector(move || {
                            arrangement::selector("jump-to-latest", key.view).to_string()
                        })
                        .on_click(self.scoped_listener(
                            cx,
                            move |this, _, window, cx| {
                                this.activate_latest(key, window, cx);
                            },
                        )),
                    ),
                ))
        })
    }

    /// What a widget's inputs and Buttons do: write to its data model, or press, in the
    /// session the view shows.
    fn widget_control(&self, key: ViewKey, index: usize, call: &str) -> widget::WidgetControl {
        let session = key.session;
        let state = self.state.downgrade();
        let (edit_call, press_call) = (call.to_owned(), call.to_owned());
        let press_state = state.clone();
        widget::WidgetControl {
            on_edit: Rc::new(move |pointer, value, cx| {
                let _ = state.update(cx, |state, cx| {
                    state.edit_widget(session, &edit_call, pointer, value, cx)
                });
            }),
            on_press: Rc::new(move |component, cx| {
                let _ = press_state.update(cx, |state, cx| {
                    state.press_widget(session, &press_call, component, cx)
                });
            }),
            selector: arrangement::selector(&format!("widget-{index}"), key.view).to_string(),
        }
    }

    /// One list entry: a row, or after the last row, the turn's status. Each entry is
    /// laid out on its own, so it carries the transcript's width and text style.
    fn transcript_entry(
        &self,
        key: ViewKey,
        index: usize,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let session = key.session;
        let Some(conversation) = self.state.read(cx).conversations().get(&session) else {
            return div().into_any_element();
        };
        let keyboard = self.presentation.current.views.keyboard_state(key);
        let selected = keyboard.is_some_and(|k| k.row == Some(index));
        let action = keyboard
            .filter(|_| selected)
            .and_then(|k| k.action.map(|action| (action, k.action_focus.clone())));
        let focused = selected
            && (keyboard.is_some_and(|k| k.action_focus.is_focused(window))
                || self.presentation.current.views.focused_region(key, window));
        let content = match conversation.rows.get(index) {
            Some(row) if conversation::visible(row) => {
                let result = match row {
                    Row::Item(item) if item.kind == ItemKind::UserMessage => {
                        let state = self.state.read(cx);
                        state
                            .result_source(session, &item.text)
                            .and_then(|child| state.session(child))
                            .map(|child| gpui::SharedString::from(child.title().to_owned()))
                    }
                    _ => None,
                };
                let reply = match row {
                    Row::Item(item) if item.kind == ItemKind::AgentMessage => {
                        Some(self.replies.borrow_mut().document((session, index), item))
                    }
                    // A result's reply is the launched session's Markdown.
                    Row::Item(item) if result.is_some() => app_state::delivery::parse(&item.text)
                        .map(|read| {
                            let reply = keystone_agent::Item {
                                text: read.reply.to_owned(),
                                status: keystone_agent::ItemStatus::Completed,
                                ..item.clone()
                            };
                            self.replies.borrow_mut().document((session, index), &reply)
                        }),
                    _ => None,
                };
                let activity = match row {
                    Row::Item(item) if transcript::activity_name(item).is_some() => {
                        let expanded = self
                            .presentation
                            .current
                            .views
                            .activity_expanded(key, index);
                        Some(transcript::ActivityControl {
                            expanded,
                            selector: arrangement::selector(
                                &format!(
                                    "activity-{}-{index}",
                                    if expanded { "expanded" } else { "collapsed" }
                                ),
                                key.view,
                            )
                            .to_string(),
                            body_selector: arrangement::selector(
                                &format!("activity-body-{index}"),
                                key.view,
                            )
                            .to_string(),
                            on_toggle: Box::new(self.scoped_listener(
                                cx,
                                move |this, _, window, cx| {
                                    this.toggle_activity(key, index, window, cx)
                                },
                            )),
                        })
                    }
                    _ => None,
                };
                let selection_start = if reply.is_some() {
                    let activation = self.presentation.activation();
                    let entity = cx.weak_entity();
                    Some(Box::new(move |window: &mut Window, cx: &mut App| {
                        entity
                            .update(cx, |this, cx| {
                                if !this.presentation.matches(activation) {
                                    return None;
                                }
                                this.begin_text_selection(key, index, window, cx)
                            })
                            .ok()
                            .flatten()
                    })
                        as Box<dyn Fn(&mut Window, &mut App) -> Option<FocusHandle>>)
                } else {
                    None
                };
                let widget = match row {
                    Row::Surface(surface) => Some(self.widget_control(key, index, &surface.call)),
                    _ => None,
                };
                Some(transcript::row(
                    row,
                    index,
                    reply,
                    transcript::RowControls {
                        action,
                        selected: focused,
                        activity,
                        selection_start,
                        widget,
                        result,
                    },
                    cx,
                ))
            }
            Some(_) => None,
            None => self.transcript_status(key, conversation, cx),
        };
        let Some(content) = content else {
            // Nothing to show takes no space, gap included.
            return div().into_any_element();
        };
        let name = match conversation.rows.len() {
            count if index < count => format!("transcript-row-{index}"),
            _ => "transcript-status".to_owned(),
        };
        let app = cx.weak_entity();
        let activation = self.presentation.activation();
        h_flex()
            .automation_id(arrangement::selector(&name, key.view))
            .relative()
            .w_full()
            .justify_center()
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let visible = bounds.intersect(&window.content_mask().bounds);
                        if visible.size.width > px(0.) && visible.size.height > px(0.) {
                            let _ = app.update(cx, |this, _cx| {
                                if this.presentation.matches(activation) {
                                    this.presentation.current.views.note_visible_row(
                                        key,
                                        index,
                                        visible.size.height >= bounds.size.height,
                                        (visible.top() - bounds.top()).max(px(0.)),
                                    );
                                }
                            });
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(
                div()
                    .w_full()
                    .max_w(px(CONTENT_WIDTH))
                    .when(focused, |d| {
                        d.relative()
                            .automation_id(arrangement::selector(
                                &format!("transcript-keyboard-row-{index}"),
                                key.view,
                            ))
                            .child(
                                canvas(
                                    |_, _, _| {},
                                    |bounds, _, window, cx| {
                                        window.paint_quad(gpui::outline(
                                            bounds,
                                            cx.theme().colors.border_focused,
                                            gpui::BorderStyle::Solid,
                                        ));
                                    },
                                )
                                .absolute()
                                .size_full(),
                            )
                    })
                    .when(index > 0, |d| d.pt_4())
                    .text_size(TextSize::Body.px())
                    .line_height(px(22.))
                    .child(content),
            )
            .into_any_element()
    }

    /// What follows the last row: an approval request or the working indicator, and any
    /// errors. `None` when there is nothing to say.
    fn transcript_status(
        &self,
        key: ViewKey,
        conversation: &Conversation,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let session = key.session;
        let colors = cx.theme().colors;
        let mut view = v_flex().gap_4();
        let mut empty = true;
        if let Some(approval) = conversation.approvals.first() {
            empty = false;
            let (allow, deny) = (approval.request.clone(), approval.request.clone());
            let name = self
                .provider(session, cx)
                .map_or("The provider", provider_name);
            view = view.child(
                v_flex()
                    .id(arrangement::selector("approval", key.view))
                    .automation_id(arrangement::selector("approval", key.view))
                    .surface(Elevation::Raised, cx)
                    .border_color(colors.border_strong)
                    .p_3()
                    .gap_2()
                    .child(
                        Label::new(match conversation.approvals.len() {
                            1 => format!("{name} asks to run"),
                            n => format!("{name} asks to run · {} more waiting", n - 1),
                        })
                        .size(TextSize::Ui)
                        .color(Color::Muted),
                    )
                    .child(
                        div()
                            .font_mono(cx)
                            .text_size(TextSize::Ui.px())
                            .child(approval.action.clone()),
                    )
                    .when_some(approval.detail.clone(), |d, detail| {
                        d.child(Label::new(detail).size(TextSize::Sm).color(Color::Muted))
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new(arrangement::selector("approve", key.view), "Allow")
                                    .style(ButtonStyle::Primary)
                                    .icon(IconName::Check)
                                    .debug_selector(move || {
                                        arrangement::selector("approve", key.view).to_string()
                                    })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.answer(session, allow.clone(), true, window, cx)
                                    })),
                            )
                            .child(
                                Button::new(arrangement::selector("deny", key.view), "Deny")
                                    .style(ButtonStyle::Subtle)
                                    .icon(IconName::Close)
                                    .debug_selector(move || {
                                        arrangement::selector("deny", key.view).to_string()
                                    })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.answer(session, deny.clone(), false, window, cx)
                                    })),
                            ),
                    ),
            );
        } else if let Some(recovery) = &conversation.recovery {
            empty = false;
            view = view.child(self.recovery_card(key, recovery, cx));
        } else if conversation.running {
            empty = false;
            view = view.child(
                h_flex()
                    .id(arrangement::selector("turn-status", key.view))
                    .automation_id(arrangement::selector("turn-status", key.view))
                    .gap_2()
                    .child(div().size(px(6.)).rounded_full().bg(colors.accent))
                    .child(
                        Label::new("Working…")
                            .size(TextSize::Ui)
                            .color(Color::Muted),
                    ),
            );
        }
        for (child, title, hold) in self.state.read(cx).waiting_results(session) {
            empty = false;
            view = view.child(
                div()
                    .automation_id(arrangement::selector(
                        &format!("result-waiting-{child}"),
                        key.view,
                    ))
                    .child(
                        Label::new(hold.notice(&title))
                            .size(TextSize::Ui)
                            .color(Color::Muted),
                    ),
            );
        }
        for (error, color) in conversation
            .load_error
            .iter()
            .chain(&conversation.error)
            .chain(&conversation.save_error)
            .map(|error| (error, Color::Danger))
            .chain(
                conversation
                    .provider_notice
                    .iter()
                    .map(|notice| (notice, Color::Muted)),
            )
        {
            empty = false;
            view = view.child(Label::new(error.clone()).size(TextSize::Ui).color(color));
        }
        (!empty).then(|| view.into_any_element())
    }

    /// Why the next message waits for recovery, with Continue and Retry where they
    /// apply. Only the view the composer sends to gives its buttons keyboard focus.
    fn recovery_card(
        &self,
        key: ViewKey,
        recovery: &conversation::Recovery,
        cx: &Context<Self>,
    ) -> AnyElement {
        use conversation::Recovery;
        let session = key.session;
        let colors = cx.theme().colors;
        let (message, color, offer_continue, offer_retry) = match recovery {
            Recovery::Loading => (
                "Loading this conversation before sending\u{2026}".to_owned(),
                Color::Muted,
                false,
                false,
            ),
            Recovery::Checking => (
                "Checking what the provider saved before sending\u{2026}".to_owned(),
                Color::Muted,
                false,
                false,
            ),
            Recovery::Saving { error: None, .. } => (
                "Saving the recovered history before sending\u{2026}".to_owned(),
                Color::Muted,
                false,
                false,
            ),
            Recovery::Saving {
                error: Some(error), ..
            } => (
                format!("The recovered history is not saved yet, so sending waits. {error}"),
                Color::Danger,
                false,
                true,
            ),
            Recovery::Review(_) => (
                "Sending is paused until you continue. Your draft is kept and nothing was resent."
                    .to_owned(),
                Color::Muted,
                true,
                true,
            ),
            Recovery::Unavailable(reason) => (
                format!(
                    "Could not check what the provider saved: {reason}. This turn's outcome is unknown. Your draft is kept and nothing was resent."
                ),
                Color::Muted,
                true,
                true,
            ),
            Recovery::Unsaved(_) => (
                "The recovered history is shown, but the log had no room to save it. Retry saves it once there is room; if you continue without it, a restart may not show it. Your draft is kept and nothing was resent."
                    .to_owned(),
                Color::Warning,
                true,
                true,
            ),
        };
        let keyboard = self.selected(cx) == Some(session)
            && self
                .presentation
                .current
                .views
                .send_target(&self.showing(session, cx), session)
                == Some(key);
        let (continue_focus, retry_focus) = self.recovery_focus.clone();
        v_flex()
            .id(arrangement::selector("recovery", key.view))
            .automation_id(arrangement::selector("recovery", key.view))
            .key_context("Recovery")
            .surface(Elevation::Raised, cx)
            .border_color(colors.border_strong)
            .p_3()
            .gap_2()
            .child(Label::new(message).size(TextSize::Ui).color(color))
            .when(offer_continue || offer_retry, |d| {
                d.child(
                    h_flex()
                        .gap_2()
                        .when(offer_continue, |d| {
                            d.child(
                                Button::new(
                                    arrangement::selector("recovery-continue", key.view),
                                    "Continue with available history",
                                )
                                .style(ButtonStyle::Primary)
                                .when(keyboard, |b| b.track_focus(&continue_focus))
                                .debug_selector(move || {
                                    arrangement::selector("recovery-continue", key.view).to_string()
                                })
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.continue_recovery(session, window, cx)
                                    },
                                )),
                            )
                        })
                        .when(offer_retry, |d| {
                            d.child(
                                Button::new(
                                    arrangement::selector("recovery-retry", key.view),
                                    "Retry",
                                )
                                .style(ButtonStyle::Subtle)
                                .when(keyboard, |b| b.track_focus(&retry_focus))
                                .debug_selector(move || {
                                    arrangement::selector("recovery-retry", key.view).to_string()
                                })
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.state.update(cx, |state, cx| {
                                            state.retry_recovery(session, cx)
                                        })
                                    },
                                )),
                            )
                        })
                        .child(
                            Label::new(match (offer_continue, offer_retry) {
                                (true, true) => {
                                    "Ctrl+Shift+Enter continues \u{b7} Ctrl+Shift+R retries"
                                }
                                (true, false) => "Ctrl+Shift+Enter continues",
                                _ => "Ctrl+Shift+R retries",
                            })
                            .size(TextSize::Sm)
                            .color(Color::Faint),
                        ),
                )
            })
            .into_any_element()
    }

    fn error_line(&self, cx: &App) -> Option<impl IntoElement> {
        self.status_line(cx).map(|(message, color)| {
            div()
                .automation_id(if self.state.read(cx).shutdown_error.is_some() {
                    "shutdown-error"
                } else {
                    "workspace-error"
                })
                .child(Label::new(message).size(TextSize::Ui).color(color))
        })
    }

    fn status_line(&self, cx: &App) -> Option<(String, Color)> {
        let state = self.state.read(cx);
        let mut errors: Vec<&str> = Vec::new();
        for error in state
            .shutdown_error
            .iter()
            .chain(&state.error)
            .chain(&self.error)
            .chain(state.log_losses())
        {
            if !errors.iter().any(|shown| shown.contains(error.as_str())) {
                errors.push(error.as_str());
            }
        }
        (!errors.is_empty())
            .then(|| (errors.join("\n"), Color::Danger))
            .or_else(|| {
                self.folder_warning
                    .as_ref()
                    .filter(|warning| {
                        self.selected(cx) == Some(warning.session)
                            && !self.state.read(cx).store().is_durable(warning.revision)
                    })
                    .map(|_| (FOLDER_UNCONFIRMED.into(), Color::Warning))
            })
    }
}

/// The session list, rendered as its own cached view. GPUI reuses its last layout and paint
/// until this entity is notified, so composer keystrokes skip every session row.
/// [`Keystone::refresh`] notifies it whenever the sessions, sections, search, or menu change.
struct SessionSidebar {
    app: WeakEntity<Keystone>,
    /// The row under the pointer. Only it and the selected row build a Settle button.
    hovered: Option<u64>,
    /// Whether the last render showed a relative time, which the minute timer must age.
    shows_time: bool,
}

/// One session as the sidebar shows it, built once per sidebar render.
struct SessionRow {
    id: u64,
    title: String,
    excerpt: String,
    provider: Provider,
    /// The session's account, named only when it is not the provider's default.
    account: Option<String>,
    settled: bool,
    selected: bool,
    /// The session's worktree branch, and whether its folder is gone.
    branch: Option<(String, bool)>,
    attention: Option<Attention>,
    /// How long ago the session last changed, or `None` for sessions saved before times.
    time: Option<String>,
}

/// The sidebar's sections, top to bottom.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    /// Unsettled sessions waiting on the user. Shown only when it has rows.
    Attention,
    /// Every other unsettled session.
    Working,
    /// Pinned to the bottom and collapsed until opened (see `Keystone::settled_open`).
    Settled,
}

impl Section {
    const ALL: [Section; 3] = [Section::Attention, Section::Working, Section::Settled];

    /// Settling is how the user puts a session away, so it outranks attention: a stale
    /// failure would otherwise pin a settled session to the top until its next message.
    fn of(row: &SessionRow) -> Self {
        match (row.settled, row.attention) {
            (true, _) => Section::Settled,
            (false, Some(_)) => Section::Attention,
            (false, None) => Section::Working,
        }
    }

    fn label(self, count: usize) -> String {
        match self {
            Section::Attention => format!("Needs action ({count})"),
            Section::Working => "Working".into(),
            Section::Settled => format!("Settled ({count})"),
        }
    }

    fn id(self) -> &'static str {
        match self {
            Section::Attention => "attention-section",
            Section::Working => "working-section",
            Section::Settled => "settled-section",
        }
    }

    fn collapsible(self) -> bool {
        self == Section::Settled
    }
}

fn attention_reason(attention: Attention) -> (&'static str, Color) {
    match attention {
        Attention::Approval => ("Waiting for approval", Color::Warning),
        Attention::Input => ("Waiting for your answer", Color::Warning),
        Attention::Failed => ("Turn failed", Color::Warning),
        Attention::Replied => ("New reply", Color::Default),
    }
}

/// The first non-blank line of a saved draft, or the provider's name when there is none.
fn excerpt(draft: &str, provider: Provider) -> String {
    draft
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(provider_name(provider))
        .to_owned()
}

impl SessionSidebar {
    /// The 12 px slot left of the title: a wedge for the session's state, or a dot
    /// for an unseen reply.
    fn state_mark(row: &SessionRow, cx: &App) -> gpui::AnyElement {
        let wedge = |name, color| {
            Icon::new(name)
                .size(px(10.))
                .color(color)
                .into_any_element()
        };
        match (row.attention, row.settled) {
            (_, true) => wedge(IconName::WedgeFilled, Color::Muted),
            (Some(Attention::Replied), false) => div()
                .size(px(10.))
                .flex()
                .items_center()
                .justify_center()
                .child(div().size(px(6.)).rounded_full().bg(cx.theme().colors.text))
                .into_any_element(),
            (Some(_), false) => wedge(IconName::WedgeFilled, Color::Warning),
            (None, false) => wedge(IconName::Wedge, Color::Accent),
        }
    }

    fn row(
        row: SessionRow,
        hovered: bool,
        app: &Entity<Keystone>,
        sidebar: &WeakEntity<Self>,
        cx: &App,
    ) -> impl IntoElement {
        let activation = app.read(cx).presentation.activation();
        let mark = Self::state_mark(&row, cx);
        let SessionRow {
            id,
            title,
            excerpt,
            provider,
            account,
            settled,
            selected,
            branch,
            attention,
            time,
        } = row;
        let title = Label::new(title).truncate();
        let title = if selected {
            title.weight(FontWeight::MEDIUM).color(Color::Strong)
        } else if settled {
            title.color(Color::Muted)
        } else {
            title
        };
        let show_settle = selected || hovered;
        let branch = branch.map(|(branch, missing)| {
            div()
                .flex_none()
                .max_w(px(120.))
                .automation_id(format!("session-branch-{id}"))
                .child(
                    Label::new(if missing {
                        "Worktree missing".to_owned()
                    } else {
                        branch
                    })
                    .size(TextSize::Sm)
                    .color(if missing {
                        Color::Warning
                    } else {
                        Color::Muted
                    })
                    .truncate(),
                )
        });
        let detail = match attention.filter(|_| !settled) {
            Some(attention) => {
                let (reason, color) = attention_reason(attention);
                h_flex()
                    .gap(px(6.))
                    .child(Label::new(reason).size(TextSize::Sm).color(color))
                    .children(branch)
            }
            None => h_flex()
                .gap(px(6.))
                .child(
                    Icon::new(provider_icon(provider))
                        .size(px(14.))
                        .color(provider_icon_color(provider, cx)),
                )
                .children(account.map(|account| {
                    div()
                        .flex_none()
                        .max_w(px(96.))
                        .debug_selector(move || format!("session-account-{id}"))
                        .child(
                            Label::new(account)
                                .size(TextSize::Sm)
                                .color(Color::Muted)
                                .truncate(),
                        )
                }))
                .children(branch)
                .child(
                    Label::new(excerpt)
                        .size(TextSize::Sm)
                        .color(Color::Muted)
                        .truncate(),
                ),
        };
        let (select, open_tab, settle, sidebar) = (
            app.downgrade(),
            app.downgrade(),
            app.downgrade(),
            sidebar.clone(),
        );
        ListItem::new(("session", id))
            .on_hover(move |&hovered, _, cx| {
                let _ = sidebar.update(cx, |this, cx| {
                    if hovered {
                        this.hovered = Some(id);
                    } else if this.hovered == Some(id) {
                        this.hovered = None;
                    }
                    cx.notify();
                });
            })
            .debug_selector(move || format!("session-{id}"))
            .selected(selected)
            .on_click(move |event, _, cx| {
                let _ = select.update(cx, |this, cx| {
                    if !this.presentation.matches(activation) {
                        return;
                    }
                    if event.modifiers().control {
                        this.open_in_new_tab(id, cx);
                    } else {
                        this.select(id, cx);
                    }
                });
            })
            .on_aux_click(move |event, _, cx| {
                let _ = open_tab.update(cx, |this, cx| {
                    if !this.presentation.matches(activation) {
                        return;
                    }
                    if event.is_middle_click() {
                        this.open_in_new_tab(id, cx);
                    } else if event.is_right_click() {
                        this.open_context_menu(id, event.position(), cx);
                    }
                });
            })
            .child(
                h_flex()
                    .gap(px(9.))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(4.))
                            .child(
                                h_flex()
                                    // A 10 px mark centred in a 12 px state slot.
                                    .pl_px()
                                    .gap(px(10.))
                                    .child(mark)
                                    .child(div().flex_1().min_w_0().child(title))
                                    // The Settle button takes this slot while it shows.
                                    .when_some(time.filter(|_| !show_settle), |d, time| {
                                        d.child(
                                            Label::new(time)
                                                .size(TextSize::Sm)
                                                .color(Color::Faint)
                                                .tabular(),
                                        )
                                    }),
                            )
                            // Under the title, past the 12 px state slot and its gap.
                            .child(detail.pl(px(21.))),
                    )
                    .when(show_settle, |d| {
                        d.child(
                            Button::new(
                                ("settle", id),
                                if settled { "Un-settle" } else { "Settle" },
                            )
                            .size(ButtonSize::Compact)
                            .style(ButtonStyle::Subtle)
                            .icon(IconName::WedgeFilled)
                            .debug_selector(move || format!("settle-{id}"))
                            .tooltip(Tooltip::text(if settled {
                                "Return to active work"
                            } else {
                                "Move out of active work"
                            }))
                            .on_click(move |_, _, cx| {
                                let _ = settle.update(cx, |this, cx| {
                                    if this.presentation.matches(activation) {
                                        this.settle(id, !settled, cx);
                                    }
                                });
                            }),
                        )
                    }),
            )
    }

    fn header(
        section: Section,
        count: usize,
        open: bool,
        app: &Entity<Keystone>,
        cx: &App,
    ) -> impl IntoElement {
        let colors = cx.theme().colors;
        let activation = app.read(cx).presentation.activation();
        let app = app.downgrade();
        h_flex()
            .id(section.id())
            .automation_id(section.id())
            .flex_shrink_0()
            .h(px(26.))
            .pl_2()
            .pr(px(6.))
            .gap_2()
            .rounded(radius::CONTROL)
            .when(section == Section::Settled, |d| {
                d.child(
                    Icon::new(IconName::WedgeFilled)
                        .size(px(10.))
                        .color(Color::Faint),
                )
            })
            .child(
                Label::new(section.label(count))
                    .size(TextSize::Sm)
                    .weight(FontWeight::MEDIUM)
                    .color(Color::Faint),
            )
            .child(div().flex_1().h_px().bg(colors.border))
            .child(
                Icon::new(if open {
                    IconName::Chevron
                } else {
                    IconName::Right
                })
                .size(px(12.))
                .color(Color::Faint),
            )
            .when(section.collapsible(), |d| {
                d.cursor_pointer()
                    .hover(move |s| s.bg(colors.element_hover))
                    .on_click(move |_, _, cx| {
                        let _ = app.update(cx, |this, cx| {
                            if !this.presentation.matches(activation) {
                                return;
                            }
                            this.settled_open = !this.settled_open;
                            this.refresh(cx);
                        });
                    })
            })
    }

    fn sections(
        &mut self,
        app: &Entity<Keystone>,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let sidebar = cx.entity().downgrade();
        let this = app.read(cx);
        let workspace = this.state.read(cx).store().workspace();
        let query = this.search.read(cx).value().to_lowercase();
        let provider_filter = this.provider_filter;
        let now = LocalTime::at(now_ms());
        let mut sections: [Vec<SessionRow>; 3] = Default::default();
        let accounts = workspace.accounts();
        let project = this.presentation.project.as_ref().ok();
        let in_project =
            |s: &&keystone_core::Session| project.is_none_or(|project| projects::holds(project, s));
        let project_empty = !workspace.sessions().iter().any(|s| in_project(&s));
        for (session, account) in workspace
            .sessions()
            .iter()
            .filter(in_project)
            .map(|s| {
                let account = s.account().and(model_selector::account_label(
                    s.provider(),
                    s.account(),
                    accounts,
                ));
                (s, account)
            })
            .filter(|(s, account)| {
                provider_filter.is_none_or(|p| s.provider() == p)
                    && format!(
                        "{} {} {} {}",
                        s.title(),
                        s.provider(),
                        account.as_deref().unwrap_or_default(),
                        s.worktree().map_or("", |w| w.branch.as_str())
                    )
                    .to_lowercase()
                    .contains(&query)
            })
        {
            let live = this.state.read(cx).conversations().get(&session.id());
            let row = SessionRow {
                id: session.id(),
                title: session.title().to_owned(),
                excerpt: excerpt(session.draft(), session.provider()),
                provider: session.provider(),
                account,
                settled: session.settled(),
                selected: Some(session.id()) == this.selected(cx),
                branch: worktrees::branch(session),
                attention: session.attention(
                    live.is_some_and(|c| !c.approvals.is_empty()),
                    live.is_some_and(Conversation::awaiting_widget),
                    live.is_some_and(|c| c.running),
                ),
                time: session
                    .updated_at()
                    .map(|then| relative(now, LocalTime::at(then))),
            };
            sections[Section::of(&row) as usize].push(row);
        }
        let empty = match (
            sections.iter().all(Vec::is_empty),
            workspace.sessions().is_empty(),
            project_empty,
        ) {
            (false, _, _) => None,
            (true, true, _) => {
                Some("No sessions yet. Start one from the composer or press Ctrl N.")
            }
            (true, false, true) => Some(
                "No sessions in this project yet. Start one from the composer or press Ctrl N.",
            ),
            (true, false, false) => Some("No sessions match this search."),
        };
        let settled_open = this.settled_open;
        self.shows_time = false;
        let mut list = v_flex()
            .id("session-list")
            .size_full()
            .overflow_y_scroll()
            .px_2()
            .py(px(6.));
        for (section, rows) in Section::ALL.into_iter().zip(sections) {
            let message = empty.filter(|_| section == Section::Working);
            if rows.is_empty() && message.is_none() {
                continue;
            }
            let open = !section.collapsible() || settled_open;
            if open {
                // The selected row shows Settle where its time would be.
                self.shows_time |= rows.iter().any(|row| row.time.is_some() && !row.selected);
            }
            let header = Self::header(section, rows.len(), open, app, cx);
            list = list
                // Spare height sits above Settled, which keeps it at the bottom.
                .when(section == Section::Settled, |d| {
                    d.child(div().flex_grow(1.))
                })
                .child(
                    v_flex()
                        .gap(px(2.))
                        .child(header)
                        .when_some(message, |d, message| {
                            d.child(
                                div()
                                    .px_2()
                                    .py(px(6.))
                                    .text_size(TextSize::Ui.px())
                                    .line_height(px(18.))
                                    .text_color(cx.theme().colors.text_muted)
                                    .child(message),
                            )
                        })
                        .when(open, |d| {
                            d.children(rows.into_iter().map(|row| {
                                let hovered = self.hovered == Some(row.id);
                                Self::row(row, hovered, app, &sidebar, cx)
                            }))
                        }),
                );
        }
        list
    }
}

impl Render for SessionSidebar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match self.app.upgrade() {
            Some(app) => self.sections(&app, cx).into_any_element(),
            None => div().into_any_element(),
        }
    }
}

impl Render for Keystone {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.focus_project_transition(window, cx);
        self.sync_title(window);
        let theme = cx.theme();
        let (background, text) = (theme.colors.background, theme.colors.text);
        let current = self
            .state
            .read(cx)
            .store()
            .workspace()
            .sessions()
            .iter()
            .find(|s| Some(s.id()) == self.selected(cx));
        let session = current.map(|s| (s.title().to_owned(), s.settled(), worktrees::branch(s)));
        let selected = current.map(|s| s.id());
        if let Some(id) = selected {
            self.state.update(cx, |state, _| {
                state.ensure_history(id);
            });
        }
        self.follow_selection(cx);
        self.update_arrangement_fitting(window, cx);
        self.reconcile_transcript_keyboard(window, cx);
        let main = match self.settings_page(cx) {
            Some(page) => page,
            None => self.arrangement_body(window, cx),
        };
        self.activate_focused_view(window, cx);
        self.note_seen(window, cx);
        let customizing = self.presentation.current.customization.is_open();
        let root = v_flex()
            .id("keystone-root")
            .size_full()
            .bg(background)
            .font_ui(cx)
            .text_color(text)
            .text_size(TextSize::Base.px())
            .track_focus(&self.focus)
            .key_context(if customizing {
                "Keystone Customize"
            } else {
                "Keystone"
            })
            .when(automation::enabled(), |d| {
                d.child(automation::titled_frame_writer(windows::title(
                    self.window_id,
                )))
            })
            .on_key_down(self.scoped_listener(cx, |this, event, window, cx| {
                if this.page_key(event, window, cx) {
                    cx.stop_propagation();
                }
            }))
            // A tab dropped over the desktop, which the platform reports as a release
            // outside the window it left, opens in a new window. A plain release outside,
            // where the platform never carried the drag, may be over another app.
            .on_mouse_up_out(
                MouseButton::Left,
                self.scoped_listener(cx, |this, _: &gpui::MouseUpEvent, _, cx| {
                    if !cx.active_drag_returned_from_platform() {
                        return;
                    }
                    let Some(view) = cx
                        .active_drag_value::<DraggedTab>()
                        .and_then(|drag| this.dragged_view(drag))
                    else {
                        return;
                    };
                    let source = cx.entity();
                    cx.defer(move |cx| {
                        windows::move_tab(
                            &source,
                            view,
                            windows::Destination::NewWindow,
                            &source,
                            cx,
                        )
                    });
                }),
            )
            // Capture runs before the focused input, so Escape closes an open menu first.
            .capture_key_down(self.scoped_listener(
                cx,
                |this, event: &gpui::KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape"
                        && cx.active_drag_value::<DraggedTab>().is_some()
                        && cx.stop_active_drag(window)
                    {
                        cx.stop_propagation();
                        return;
                    }
                    if event.keystroke.key != "escape"
                        || this.renaming.is_some()
                        || this.worktree_dialog.is_some()
                        || this.confirm_full_access.is_some()
                        || this.settings.is_some()
                    {
                        return;
                    }
                    if this.presentation.current.customization.chooser().is_some() {
                        this.customize(arrangement_editor::Action::CloseChooser, window, cx);
                        cx.stop_propagation();
                    } else if this.context_menu.take().is_some() {
                        cx.notify();
                        cx.stop_propagation();
                    } else if this.new_menu
                        || this.filter_menu
                        || this.approval_menu
                        || this.model_menu.is_some()
                        || this.presentation.current.arrangement.surface_menu.is_some()
                    {
                        this.new_menu = false;
                        this.filter_menu = false;
                        this.approval_menu = false;
                        this.model_menu = None;
                        this.presentation.current.arrangement.surface_menu = None;
                        this.refresh(cx);
                        cx.stop_propagation();
                    } else if this.dismiss_slash(cx) || this.dismiss_mention(cx) {
                        cx.stop_propagation();
                    } else if this.presentation.current.customization.is_open() {
                        this.customize(arrangement_editor::Action::Close, window, cx);
                        cx.stop_propagation();
                    }
                },
            ))
            .on_action(
                self.scoped_listener(cx, |this, _: &files::ToggleFiles, window, cx| {
                    this.toggle_files(window, cx)
                }),
            )
            .on_action(
                self.scoped_listener(cx, |this, _: &notes::ToggleNotes, window, cx| {
                    this.toggle_surface(
                        keystone_core::Component::Notes,
                        notes::NOTES_WIDTH,
                        window,
                        cx,
                    )
                }),
            )
            .on_action(
                self.scoped_listener(cx, |this, _: &changes::ToggleChanges, window, cx| {
                    this.toggle_changes(window, cx)
                }),
            )
            .on_action(self.scoped_listener(
                cx,
                |this, _: &worktrees::NewWorktreeSession, window, cx| {
                    this.open_new_worktree(window, cx)
                },
            ))
            .on_action(self.scoped_listener(
                cx,
                |this, _: &worktrees::RemoveWorktree, window, cx| match this.selected(cx) {
                    Some(session) => this.open_remove_worktree(session, window, cx),
                    None => {
                        this.error = Some("No session is open here.".into());
                        this.refresh(cx);
                    }
                },
            ))
            .on_action(self.scoped_listener(
                cx,
                |this, _: &customization::OpenCustomize, window, cx| {
                    this.customize(
                        if this.presentation.current.customization.is_open() {
                            arrangement_editor::Action::Close
                        } else {
                            arrangement_editor::Action::Open
                        },
                        window,
                        cx,
                    )
                },
            ))
            .on_action(self.scoped_listener(
                cx,
                |this, _: &customization::ApplyLayout, window, cx| {
                    this.customize(arrangement_editor::Action::Apply, window, cx)
                },
            ))
            .on_action(
                self.scoped_listener(cx, |this, _: &customization::MoveLeft, window, cx| {
                    this.move_component(keystone_core::Axis::Row, false, window, cx)
                }),
            )
            .on_action(self.scoped_listener(
                cx,
                |this, _: &customization::MoveRight, window, cx| {
                    this.move_component(keystone_core::Axis::Row, true, window, cx)
                },
            ))
            .on_action(
                self.scoped_listener(cx, |this, _: &customization::MoveUp, window, cx| {
                    this.move_component(keystone_core::Axis::Column, false, window, cx)
                }),
            )
            .on_action(
                self.scoped_listener(cx, |this, _: &customization::MoveDown, window, cx| {
                    this.move_component(keystone_core::Axis::Column, true, window, cx)
                }),
            )
            .on_action(
                self.scoped_listener(cx, |this, _: &customization::Narrower, window, cx| {
                    this.size_component(-1, None, window, cx)
                }),
            )
            .on_action(
                self.scoped_listener(cx, |this, _: &customization::Wider, window, cx| {
                    this.size_component(1, None, window, cx)
                }),
            )
            .on_action(
                self.scoped_listener(cx, |_, _: &NextFocus, window, cx| window.focus_next(cx)),
            )
            .on_action(
                self.scoped_listener(cx, |_, _: &PreviousFocus, window, cx| window.focus_prev(cx)),
            )
            .on_action(
                self.scoped_listener(cx, |this, _: &OpenSettings, window, cx| {
                    this.toggle_settings(window, cx)
                }),
            )
            .on_action(self.scoped_listener(cx, |this, _: &NewClaude, window, cx| {
                this.start(Provider::Claude, window, cx)
            }))
            .on_action(self.scoped_listener(cx, |this, _: &NewCodex, window, cx| {
                this.start(Provider::Codex, window, cx)
            }))
            .on_action(
                self.scoped_listener(cx, |this, _: &ToggleSettle, _, cx| this.toggle_settle(cx)),
            )
            .on_action(
                self.scoped_listener(cx, |this, _: &ContinueRecovery, window, cx| {
                    if let Some(id) = this.selected_session(cx).map(|s| s.id()) {
                        this.continue_recovery(id, window, cx)
                    }
                }),
            )
            .on_action(self.scoped_listener(cx, |this, _: &RetryRecovery, _, cx| {
                if let Some(id) = this.selected_session(cx).map(|s| s.id()) {
                    this.state
                        .update(cx, |state, cx| state.retry_recovery(id, cx));
                }
            }))
            .on_action(
                self.scoped_listener(cx, |this, _: &FocusComposer, window, cx| {
                    this.focus_composer(window, cx)
                }),
            )
            .on_action(self.scoped_listener(
                cx,
                |this, _: &transcript_keyboard::FocusTranscript, window, cx| {
                    this.focus_transcript(window, cx)
                },
            ))
            .on_action(self.scoped_listener(
                cx,
                |this, _: &transcript_keyboard::NextTranscript, window, cx| {
                    this.cycle_transcript(false, window, cx)
                },
            ))
            .on_action(self.scoped_listener(
                cx,
                |this, _: &transcript_keyboard::PreviousTranscript, window, cx| {
                    this.cycle_transcript(true, window, cx)
                },
            ))
            .on_action(
                self.scoped_listener(cx, |this, _: &SendMessage, window, cx| {
                    if let Some(draft) = this.composer_draft(cx) {
                        this.send_message(draft, window, cx)
                    }
                }),
            )
            .on_action(
                self.scoped_listener(cx, |this, _: &RenameSession, window, cx| {
                    this.open_rename(window, cx)
                }),
            )
            .on_action(
                self.scoped_listener(cx, |this, _: &handoff::HandOff, window, cx| {
                    this.hand_off(window, cx)
                }),
            )
            .on_action(
                self.scoped_listener(cx, |this, _: &FocusSearch, window, cx| {
                    if !this.arrangement_has_sidebar(cx) {
                        this.presentation.current.arrangement.sidebar_overlay = false;
                        return;
                    }
                    this.presentation.current.sidebar = true;
                    if this.presentation.current.arrangement.fitted {
                        this.presentation.current.arrangement.sidebar_overlay = true;
                    }
                    this.search.focus_handle(cx).focus(window, cx);
                    this.refresh(cx);
                }),
            )
            .on_action(
                self.scoped_listener(cx, |this, _: &ToggleSidebar, window, cx| {
                    this.toggle_sidebar(window, cx)
                }),
            )
            .on_action(
                self.scoped_listener(cx, |this, _: &OpenPalette, window, cx| {
                    this.toggle_palette(window, cx)
                }),
            )
            .on_action(
                self.scoped_listener(cx, |this, _: &projects::SwitchProject, window, cx| {
                    this.open_projects(None, window, cx)
                }),
            )
            .on_action(
                self.scoped_listener(cx, |this, _: &projects::OpenProject, window, cx| {
                    this.choose_project(window, cx)
                }),
            )
            .on_action(self.scoped_listener(
                cx,
                |this, _: &attachments::AttachFiles, window, cx| {
                    this.choose_attachments(window, cx)
                },
            ))
            .on_action(
                self.scoped_listener(cx, |this, _: &prompts::InsertPrompt, window, cx| {
                    this.open_prompt_list(window, cx)
                }),
            )
            .on_action(
                self.scoped_listener(cx, |this, _: &prompts::InsertPrompt, window, cx| {
                    this.open_prompt_list(window, cx)
                }),
            )
            .on_action(
                self.scoped_listener(cx, |this, _: &session_tabs::NewTab, window, cx| {
                    this.new_tab(window, cx)
                }),
            )
            .on_action(
                self.scoped_listener(cx, |this, _: &session_tabs::CloseTab, window, cx| {
                    this.close_active_tab(window, cx)
                }),
            )
            .on_action(
                self.scoped_listener(cx, |this, _: &session_tabs::NextTab, window, cx| {
                    this.cycle_tab(false, window, cx)
                }),
            )
            .on_action(
                self.scoped_listener(cx, |this, _: &session_tabs::PreviousTab, window, cx| {
                    this.cycle_tab(true, window, cx)
                }),
            )
            .on_action(
                self.scoped_listener(cx, |this, _: &session_tabs::SplitRight, window, cx| {
                    this.split_right(window, cx)
                }),
            )
            .on_action(
                self.scoped_listener(cx, |this, _: &windows::NewWindow, window, cx| {
                    this.open_new_window(None, window, cx)
                }),
            )
            .on_action(
                self.scoped_listener(cx, |_, _: &windows::CloseWindow, window, cx| {
                    let root = cx.weak_entity();
                    window.defer(cx, move |window, cx| {
                        if close_window(&root, window, cx) {
                            window.remove_window();
                        }
                    });
                }),
            )
            .on_action(self.scoped_listener(
                cx,
                |this, _: &windows::MoveTabToWindow, window, cx| {
                    this.open_move_tab_palette(window, cx)
                },
            ))
            .on_action(self.scoped_listener(
                cx,
                |this, _: &windows::OpenInNewWindow, window, cx| {
                    if let Some(session) = this.selected(cx) {
                        this.open_new_window(Some(session), window, cx)
                    }
                },
            ))
            .child(self.top_bar(session.filter(|_| self.settings.is_none()), cx))
            .when(customizing, |d| {
                d.child(self.customize_panel(window.viewport_size(), cx))
            })
            .child(main)
            .children(self.borrow_dialog(cx))
            .children(self.full_access_dialog(cx))
            .children(self.rename_dialog(cx))
            .children(self.worktree_dialog_view(cx))
            .children(self.palette_overlay(cx))
            .children(self.projects_overlay(cx))
            .children(self.prompt_list_overlay(cx))
            .children(self.context_menu_overlay(cx));
        let root = self.settings_yields_to_workspace(root, cx);
        self.on_arrangement_drag(root, cx)
    }
}

#[cfg(all(test, feature = "ui-tests"))]
mod shutdown_tests;

#[cfg(all(test, feature = "ui-tests"))]
mod feedback_tests;

#[cfg(all(test, feature = "ui-tests"))]
mod tests {
    #[gpui::test]
    fn transcript_selection_survives_wheel_until_its_row_is_hidden(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = open_with_session(dir.path(), cx);
        wait_until(cx, &view, |v, cx| {
            !v.state.read(cx).conversations()[&1].is_loading()
        });
        view.update(cx, |v, cx| {
            v.state.update(cx, |state, _| {
                let conversation = state.test_conversation(1);
                conversation.apply(Event::ItemCompleted(Item {
                    id: "wheel-selection".into(),
                    kind: ItemKind::AgentMessage,
                    text: format!(
                        "Plain **bold** words here.\n\n{}",
                        "More text.\n\n".repeat(60)
                    ),
                    status: ItemStatus::Completed,
                }));
                for n in 0..30 {
                    conversation.push_local(&format!("Later message {n}"));
                }
            });
            v.commit(1, cx);
            cx.notify();
        });
        cx.simulate_resize(size(px(1000.), px(500.)));
        for pointer in [true, false] {
            activity_keys(cx, "ctrl-shift-t home");
            if pointer {
                let bounds = cx.debug_bounds("reply-0-selection-surface").unwrap();
                let from = bounds.origin + point(px(1.), px(8.));
                let to = from + point(px(300.), px(0.));
                cx.simulate_mouse_down(from, MouseButton::Left, gpui::Modifiers::none());
                cx.simulate_mouse_move(to, MouseButton::Left, gpui::Modifiers::none());
                cx.simulate_mouse_up(to, MouseButton::Left, gpui::Modifiers::none());
            } else {
                activity_keys(cx, "tab tab ctrl-a left shift-end");
            }
            cx.update(|_, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string("sentinel".into()))
            });
            let over = cx.debug_bounds("conversation").unwrap().center();
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: over,
                delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-20.))),
                ..Default::default()
            });
            cx.run_until_parked();
            activity_keys(cx, "ctrl-c");
            assert_eq!(
                cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()))
                    .as_deref(),
                Some("Plain bold words here."),
                "pointer={pointer}"
            );
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: over,
                delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-10000.))),
                ..Default::default()
            });
            cx.run_until_parked();
            view.update_in(cx, |v, window, cx| {
                assert!(v.presentation.current.views.focused_region(MAIN_1, window));
                assert_eq!(
                    v.presentation.current.views.keyboard(MAIN_1, cx).action,
                    None
                );
            });
            activity_keys(cx, "ctrl-shift-t home tab tab");
            cx.update(|_, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string("sentinel".into()))
            });
            activity_keys(cx, "ctrl-c");
            assert_eq!(
                cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()))
                    .as_deref(),
                Some("sentinel")
            );
        }
    }

    #[gpui::test]
    fn collapsed_tool_preserves_its_lifecycle_status(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = open_with_session(dir.path(), cx);
        wait_until(cx, &view, |v, cx| {
            !v.state.read(cx).conversations()[&1].is_loading()
        });
        for status in [
            ItemStatus::InProgress,
            ItemStatus::Failed,
            ItemStatus::Declined,
        ] {
            view.update(cx, |v, cx| {
                v.state.update(cx, |state, _| {
                    state.test_conversation(1).apply(Event::ItemCompleted(Item {
                        id: "tool-status".into(),
                        kind: ItemKind::Tool {
                            name: "Read".into(),
                        },
                        text: "Read src/main.rs".into(),
                        status,
                    }));
                });
                v.commit(1, cx);
                cx.notify();
            });
            cx.run_until_parked();
            if cx.debug_bounds("activity-expanded-0").is_some() {
                click(cx, "activity-expanded-0");
            }
            assert!(cx.debug_bounds("activity-collapsed-0").is_some());
            assert!(cx.debug_bounds("activity-body-0").is_none());
            assert!(cx.debug_bounds("activity-status-0").is_some(), "{status:?}");
        }
    }

    #[gpui::test]
    fn transcript_command_disclosure_fits_minimum_width(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = open_with_session(dir.path(), cx);
        wait_until(cx, &view, |v, cx| {
            !v.state.read(cx).conversations()[&1].is_loading()
        });
        view.update(cx, |v, cx| {
            v.state.update(cx, |state, _| {
                state.test_conversation(1).apply(Event::ItemCompleted(Item {
                    id: "narrow-command".into(),
                    kind: ItemKind::Command {
                        command: "cargo test --workspace --all-targets".into(),
                        exit_code: Some(0),
                    },
                    text: "one output line".into(),
                    status: ItemStatus::Completed,
                }));
            });
            v.commit(1, cx);
            cx.notify();
        });
        for width in [400., 360., 330.] {
            cx.simulate_resize(size(px(width), px(700.)));
            cx.run_until_parked();
            let row = cx.debug_bounds("transcript-row-0").unwrap();
            let copy = cx.debug_bounds("copy-command-0").unwrap();
            let toggle = cx.debug_bounds("activity-expanded-0").unwrap();
            assert!(row.size.height < px(140.), "{width}px: {row:?}");
            assert!(copy.right() <= row.right() && toggle.right() <= row.right());
            assert!(
                copy.left() < toggle.left(),
                "visual order follows Copy-first keyboard order"
            );
        }
    }

    #[gpui::test]
    fn transcript_selection_does_not_cross_keyboard_created_sessions(
        cx: &mut gpui::TestAppContext,
    ) {
        let _env = codex_env();
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = open_with_session(dir.path(), cx);
        wait_until(cx, &view, |v, cx| {
            !v.state.read(cx).conversations()[&1].is_loading()
        });
        view.update(cx, |v, cx| {
            v.state.update(cx, |state, _| {
                state.test_conversation(1).apply(Event::ItemCompleted(Item {
                    id: "session-one".into(),
                    kind: ItemKind::AgentMessage,
                    text: "Plain **bold** words here.".into(),
                    status: ItemStatus::Completed,
                }));
            });
            v.commit(1, cx);
            cx.notify();
        });
        activity_keys(cx, "ctrl-shift-t end tab tab ctrl-a ctrl-c ctrl-shift-n");
        wait_until(cx, &view, |v, cx| {
            v.state.read(cx).store().workspace().selected() == Some(2)
                && v.state
                    .read(cx)
                    .conversations()
                    .get(&2)
                    .is_some_and(|c| !c.is_loading())
        });
        view.update(cx, |v, cx| {
            v.state.update(cx, |state, _| {
                state.test_conversation(2).apply(Event::ItemCompleted(Item {
                    id: "session-two".into(),
                    kind: ItemKind::AgentMessage,
                    text: "Plain **bold** words here. appended".into(),
                    status: ItemStatus::Completed,
                }));
            });
            v.commit(2, cx);
            cx.notify();
        });
        activity_keys(cx, "ctrl-shift-t end tab tab");
        cx.update(|_, cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string("sentinel".into()))
        });
        activity_keys(cx, "ctrl-c");
        assert_eq!(
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()))
                .as_deref(),
            Some("sentinel")
        );
        activity_keys(cx, "ctrl-a ctrl-c");
        assert_eq!(
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()))
                .as_deref(),
            Some("Plain bold words here. appended")
        );
    }

    fn activity_keys(cx: &mut gpui::VisualTestContext, strokes: &str) {
        for stroke in strokes.split_whitespace() {
            cx.simulate_keystrokes(stroke);
            cx.simulate_event(gpui::KeyUpEvent {
                keystroke: gpui::Keystroke::parse(stroke).unwrap(),
            });
            cx.run_until_parked();
        }
    }

    fn activity_top(list: &gpui::ListState) -> (usize, Pixels) {
        let top = list.logical_scroll_top();
        (top.item_ix, top.offset_in_item)
    }

    #[gpui::test]
    fn transcript_keyboard_selection_reveals_its_moving_head(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = open_with_session(dir.path(), cx);
        wait_until(cx, &view, |v, cx| {
            !v.state.read(cx).conversations()[&1].is_loading()
        });
        view.update(cx, |v, cx| {
            v.state.update(cx, |state, _| {
                state.test_conversation(1).apply(Event::ItemCompleted(Item {
                    id: "tall-selection".into(),
                    kind: ItemKind::AgentMessage,
                    text: format!(
                        "First **styled** paragraph.\n\n{}\n\n```rust\nlast code\n```",
                        "More paragraph.\n\n".repeat(120)
                    ),
                    status: ItemStatus::Completed,
                }));
            });
            v.commit(1, cx);
            cx.notify();
        });
        cx.simulate_resize(size(px(1000.), px(500.)));
        activity_keys(cx, "ctrl-shift-t end tab tab");
        let before = view.read_with(cx, |v, _| keyboard_scroll_top(v));
        activity_keys(cx, "tab ctrl-a");
        view.read_with(cx, |v, _| {
            assert_eq!(
                keyboard_scroll_top(v),
                before,
                "selection entry and Select All retain the viewport"
            )
        });
        activity_keys(cx, "left shift-end ctrl-c");
        let surface = cx.debug_bounds("reply-0-selection-surface").unwrap();
        let viewport = cx.debug_bounds("conversation").unwrap();
        assert!(
            surface.top() >= viewport.top(),
            "the moving selection head must be visible: {surface:?} in {viewport:?}"
        );
        assert_eq!(
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()))
                .as_deref(),
            Some("First styled paragraph.")
        );
    }

    #[gpui::test]
    fn transcript_pointer_selection_keeps_text_focus_in_the_host(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = open_with_session(dir.path(), cx);
        wait_until(cx, &view, |v, cx| {
            !v.state.read(cx).conversations()[&1].is_loading()
        });
        view.update(cx, |v, cx| {
            v.state.update(cx, |state, _| {
                state.test_conversation(1).apply(Event::ItemCompleted(Item {
                    id: "pointer-reply".into(),
                    kind: ItemKind::AgentMessage,
                    text: "Plain **bold** and [link](https://example.com) é.".into(),
                    status: ItemStatus::Completed,
                }));
            });
            v.commit(1, cx);
            cx.notify();
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string("sentinel".into()))
        });
        let bounds = cx.debug_bounds("reply-0-selection-surface").unwrap();
        let from = bounds.origin + point(px(1.), px(8.));
        let to = from + point(px(300.), px(0.));
        cx.simulate_mouse_down(from, MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_mouse_move(to, MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_mouse_up(to, MouseButton::Left, gpui::Modifiers::none());
        activity_keys(cx, "ctrl-c");
        assert_eq!(
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()))
                .as_deref(),
            Some("Plain bold and link é.")
        );
        view.update_in(cx, |v, window, cx| {
            let keyboard = v.presentation.current.views.keyboard(MAIN_1, cx);
            assert_eq!(keyboard.row, Some(0));
            assert_eq!(keyboard.action, Some(view_state::CopyTarget::TextSelection));
            assert!(keyboard.action_focus.is_focused(window));
        });
        assert_eq!(cx.opened_url(), None);
    }

    #[gpui::test]
    fn transcript_text_selection_is_a_bounded_action_and_keeps_row_keys(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = open_with_session(dir.path(), cx);
        wait_until(cx, &view, |v, cx| {
            !v.state.read(cx).conversations()[&1].is_loading()
        });
        view.update(cx, |v, cx| {
            v.state.update(cx, |state, _| {
                let conversation = state.test_conversation(1);
                conversation.push_local("Question");
                conversation.apply(Event::ItemCompleted(Item {
                    id: "selection-reply".into(),
                    kind: ItemKind::AgentMessage,
                    text: "Plain **bold** and [link](https://example.com) é.\n\nNext `code`."
                        .into(),
                    status: ItemStatus::Completed,
                }));
            });
            v.commit(1, cx);
            cx.notify();
        });
        activity_keys(cx, "ctrl-shift-t end tab tab ctrl-a ctrl-c");
        assert_eq!(
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()))
                .as_deref(),
            Some("Plain bold and link é.\nNext code.")
        );
        activity_keys(cx, "ctrl-a left shift-right shift-right ctrl-c");
        assert_eq!(
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()))
                .as_deref(),
            Some("Pl")
        );
        activity_keys(cx, "escape up");
        view.read_with(cx, |v, _| {
            assert_eq!(
                v.presentation
                    .current
                    .views
                    .keyboard_state(MAIN_1)
                    .unwrap()
                    .row,
                Some(0)
            )
        });
        activity_keys(cx, "ctrl-l");
        cx.simulate_input("composer still owns text");
        view.read_with(cx, |v, cx| {
            assert_eq!(v.composer.read(cx).value(), "composer still owns text")
        });
    }
    #[gpui::test]
    fn activity_disclosure_preserves_header_and_independent_stream_anchors(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = open_with_session(dir.path(), cx);
        wait_until(cx, &view, |v, cx| {
            !v.state.read(cx).conversations()[&1].is_loading()
        });
        view.update(cx, |v, cx| {
            v.state.update(cx, |state, _| {
                let conversation = state.test_conversation(1);
                for n in 0..3 {
                    conversation.apply(Event::ItemStarted(Item {
                        id: format!("activity-{n}"),
                        kind: ItemKind::Reasoning,
                        text: "Long reasoning line.\n".repeat(100),
                        status: ItemStatus::InProgress,
                    }));
                }
            });
            v.commit(1, cx);
            cx.notify();
        });
        cx.simulate_resize(size(px(1000.), px(500.)));
        activity_keys(cx, "ctrl-shift-t home");
        cx.run_until_parked();
        let before = cx.debug_bounds("activity-expanded-0").unwrap().top();
        click(cx, "activity-expanded-0");
        assert_eq!(
            cx.debug_bounds("activity-collapsed-0").unwrap().top(),
            before
        );
        click(cx, "activity-collapsed-0");
        assert_eq!(
            cx.debug_bounds("activity-expanded-0").unwrap().top(),
            before
        );
        let other = ViewKey {
            view: ViewId(919),
            session: 1,
        };
        view.update(cx, |v, cx| {
            let views = &mut v.presentation.current.views;
            let list = views.get(MAIN_1).unwrap().clone();
            list.scroll_to(ListOffset {
                item_ix: 2,
                offset_in_item: px(70.),
            });
            views.list(other, 3).scroll_to(ListOffset {
                item_ix: 1,
                offset_in_item: px(30.),
            });
            views.toggle_activity(MAIN_1, 0);
            assert_eq!(activity_top(&list), (2, px(70.)));
            assert!(views.activity_expanded(other, 0));
            assert!(!views.activity_expanded(MAIN_1, 0));
            v.state.update(cx, |state, _| {
                state.test_conversation(1).apply(Event::ItemCompleted(Item {
                    id: "activity-0".into(),
                    kind: ItemKind::Reasoning,
                    text: "New streamed reasoning.\n".repeat(120),
                    status: ItemStatus::Completed,
                }));
            });
            v.commit(1, cx);
            cx.notify();
        });
        cx.run_until_parked();
        view.update(cx, |v, cx| {
            let views = &mut v.presentation.current.views;
            assert_eq!(activity_top(views.get(MAIN_1).unwrap()), (2, px(70.)));
            assert_eq!(activity_top(views.get(other).unwrap()), (1, px(30.)));
            assert!(!views.activity_expanded(MAIN_1, 0));
            let list = views.get(MAIN_1).unwrap().clone();
            list.scroll_to(ListOffset {
                item_ix: 1,
                offset_in_item: px(400.),
            });
            views.toggle_activity(MAIN_1, 1);
            assert_eq!(activity_top(&list), (1, px(0.)));
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("activity-collapsed-1").is_some());
    }

    #[gpui::test]
    fn activity_controls_keep_keyboard_copy_and_stream_position(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = open_with_session(dir.path(), cx);
        wait_until(cx, &view, |v, cx| {
            !v.state.read(cx).conversations()[&1].is_loading()
        });
        view.update(cx, |v, cx| {
            v.state.update(cx, |state, _| {
                let conversation = state.test_conversation(1);
                for (id, kind, text) in [
                    (
                        "reason",
                        ItemKind::Reasoning,
                        "Thinking through **details**",
                    ),
                    (
                        "tool",
                        ItemKind::Tool {
                            name: "Read".into(),
                        },
                        "Read src/main.rs",
                    ),
                    (
                        "cmd",
                        ItemKind::Command {
                            command: "cargo test".into(),
                            exit_code: None,
                        },
                        "command output",
                    ),
                ] {
                    conversation.apply(Event::ItemStarted(Item {
                        id: id.into(),
                        kind,
                        text: text.into(),
                        status: ItemStatus::InProgress,
                    }));
                }
            });
            v.commit(1, cx);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("activity-expanded-0").is_some(),
            "reasoning needs a named disclosure"
        );
        assert!(cx.debug_bounds("activity-body-0").is_some());
        activity_keys(cx, "ctrl-shift-t home tab enter");
        cx.run_until_parked();
        assert!(cx.debug_bounds("activity-collapsed-0").is_some());
        assert!(cx.debug_bounds("activity-body-0").is_none());
        activity_keys(cx, "space");
        cx.run_until_parked();
        assert!(cx.debug_bounds("activity-body-0").is_some());
        activity_keys(cx, "down tab enter");
        cx.run_until_parked();
        assert!(cx.debug_bounds("activity-collapsed-1").is_some());
        activity_keys(cx, "down tab enter");
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()))
                .as_deref(),
            Some("command output")
        );
        activity_keys(cx, "tab enter");
        cx.run_until_parked();
        assert!(cx.debug_bounds("activity-collapsed-2").is_some());
        view.update(cx, |v, cx| {
            v.state.update(cx, |state, _| {
                state.test_conversation(1).apply(Event::ItemCompleted(Item {
                    id: "cmd".into(),
                    kind: ItemKind::Command {
                        command: "cargo test".into(),
                        exit_code: Some(0),
                    },
                    text: "streamed command output".into(),
                    status: ItemStatus::Completed,
                }));
            });
            v.commit(1, cx);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("activity-collapsed-2").is_some());
        cx.simulate_keystrokes("ctrl-shift-c");
        assert_eq!(
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()))
                .as_deref(),
            Some("streamed command output")
        );
        click(cx, "activity-collapsed-2");
        assert!(cx.debug_bounds("activity-body-2").is_some());
    }

    use super::*;
    use conversation::Row;
    use keystone_agent::{Event, Item, ItemKind, ItemStatus, TurnStatus};

    pub(super) fn lock_database(path: &Path) -> rusqlite::Connection {
        let connection = rusqlite::Connection::open_with_flags(
            keystone_core::database_path(path),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        )
        .unwrap();
        connection.execute_batch("BEGIN IMMEDIATE").unwrap();
        connection
    }

    fn working_directory() -> WorkingDirectory {
        WorkingDirectory::new(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    const MAIN_1: ViewKey = ViewKey {
        view: ViewId::MAIN,
        session: 1,
    };

    fn keyboard_scroll_top(view: &Keystone) -> (usize, Pixels) {
        let top = view
            .presentation
            .current
            .views
            .get(MAIN_1)
            .unwrap()
            .logical_scroll_top();
        (top.item_ix, top.offset_in_item)
    }

    #[gpui::test]
    fn transcript_keyboard_copies_virtual_rows_and_code_without_sending(
        cx: &mut gpui::TestAppContext,
    ) {
        use keystone_agent::log::{Entry, SessionLog};
        fn keys(cx: &mut gpui::VisualTestContext, strokes: &str) {
            for key in strokes.split_whitespace() {
                cx.simulate_keystrokes(key);
                cx.simulate_event(gpui::KeyUpEvent {
                    keystroke: gpui::Keystroke::parse(key).unwrap(),
                });
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1)
            .unwrap();
        seed_long_session(&dir.path().join("sessions"), 1, 200);
        let reply = "A reply\n\n```rust\nlet value = 7;\n```\n\n> ```text\n> nested\n> ```\n";
        let mut log = SessionLog::open(&dir.path().join("sessions"), 1).unwrap();
        for (id, kind, text) in [
            (
                "empty-command",
                ItemKind::Command {
                    command: "printf test".into(),
                    exit_code: Some(0),
                },
                "",
            ),
            ("code-reply", ItemKind::AgentMessage, reply),
        ] {
            log.append(&Entry::Item {
                item: Item {
                    id: id.into(),
                    kind,
                    text: text.into(),
                    status: ItemStatus::Completed,
                },
                seq: None,
                run: None,
            })
            .unwrap();
        }
        cx.update(|cx| {
            keystone_ui::init(cx);
            bind_keys(cx);
        });
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        wait_until(cx, &view, |v, cx| {
            v.state
                .read(cx)
                .conversations()
                .get(&1)
                .is_some_and(|c| c.rows.len() == 602)
        });
        let clipboard = |cx: &mut gpui::VisualTestContext| {
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()))
        };
        view.update_in(cx, |v, window, cx| v.focus.focus(window, cx));
        keys(cx, "ctrl-shift-t");
        keys(cx, "home");
        keys(cx, "ctrl-shift-c");
        assert_eq!(clipboard(cx).as_deref(), Some("Question 0"));
        assert!(cx.debug_bounds("transcript-row-0").is_some());
        assert!(cx.debug_bounds("transcript-row-599").is_none());
        keys(cx, "tab enter");
        assert_eq!(clipboard(cx).as_deref(), Some("Question 0"));
        keys(cx, "down tab space");
        assert_eq!(
            clipboard(cx).as_deref(),
            Some("running 12 tests\ntest result: ok")
        );
        keys(cx, "end tab enter");
        assert_eq!(clipboard(cx).as_deref(), Some(reply));
        keys(cx, "tab space");
        assert_eq!(clipboard(cx).as_deref(), Some("let value = 7;"));
        keys(cx, "tab enter");
        assert_eq!(clipboard(cx).as_deref(), Some("nested"));
        keys(cx, "shift-tab ctrl-shift-c");
        assert_eq!(clipboard(cx).as_deref(), Some("let value = 7;"));
        keys(cx, "up tab enter");
        assert_eq!(clipboard(cx).as_deref(), Some("printf test"));
        keys(cx, "ctrl-l");
        cx.simulate_input("draft stays");
        keys(cx, "ctrl-shift-c");
        view.read_with(cx, |v, cx| {
            assert_eq!(v.composer.read(cx).value(), "draft stays");
            assert!(!v.state.read(cx).conversations()[&1].running);
            assert_eq!(v.state.read(cx).conversations()[&1].rows.len(), 602);
        });
        keys(cx, "ctrl-shift-t home alt-end enter");
        cx.run_until_parked();
        assert!(cx.debug_bounds("jump-to-latest").is_none());
        assert!(cx.debug_bounds("transcript-row-601").is_some());
    }

    #[gpui::test]
    fn transcript_keyboard_diagrams_toggle_copy_and_preserve_code_ordinals(
        cx: &mut gpui::TestAppContext,
    ) {
        fn image(cx: &mut gpui::VisualTestContext, selector: &'static str) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
            loop {
                cx.run_until_parked();
                cx.update(|window, cx| window.draw(cx).clear(cx));
                if cx.debug_bounds(selector).is_some() {
                    return;
                }
                assert!(std::time::Instant::now() < deadline, "{selector}");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        fn keys(cx: &mut gpui::VisualTestContext, strokes: &str) {
            for key in strokes.split_whitespace() {
                cx.simulate_keystrokes(key);
                cx.simulate_event(gpui::KeyUpEvent {
                    keystroke: gpui::Keystroke::parse(key).unwrap(),
                });
                cx.run_until_parked();
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = open_with_session(dir.path(), cx);
        wait_until(cx, &view, |v, cx| {
            !v.state.read(cx).conversations()[&1].is_loading()
        });
        let reply = "```text\nbefore\n```\n\n> ```mermaid\n> flowchart TD\n> A-->B\n> ```\n\n```text\nafter\n```\n\n- ```mermaid\n  pie\n  \"A\" : 1\n  ```\n";
        view.update(cx, |v, cx| {
            v.state.update(cx, |state, _| {
                state.test_conversation(1).apply(Event::ItemCompleted(Item {
                    id: "diagrams".into(),
                    kind: ItemKind::AgentMessage,
                    text: reply.into(),
                    status: ItemStatus::Completed,
                }))
            });
            v.commit(1, cx);
            cx.notify();
        });
        let clipboard = |cx: &mut gpui::VisualTestContext| {
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()))
        };
        keys(cx, "ctrl-shift-t end tab enter");
        assert_eq!(clipboard(cx).as_deref(), Some(reply));
        keys(cx, "tab space");
        assert_eq!(clipboard(cx).as_deref(), Some("before"));
        image(cx, "reply-0-diagram-2-image");
        keys(cx, "tab space");
        assert!(cx.debug_bounds("reply-0-diagram-2-source").is_some());
        keys(cx, "ctrl-shift-c");
        assert_eq!(clipboard(cx).as_deref(), Some("flowchart TD\nA-->B\n"));
        keys(cx, "enter");
        assert!(cx.debug_bounds("reply-0-diagram-2-source").is_none());
        keys(cx, "tab enter");
        assert_eq!(clipboard(cx).as_deref(), Some("flowchart TD\nA-->B\n"));
        keys(cx, "tab space");
        assert_eq!(clipboard(cx).as_deref(), Some("after"));
        image(cx, "reply-0-diagram-4-image");
        keys(cx, "tab enter");
        assert!(cx.debug_bounds("reply-0-diagram-4-source").is_some());
        keys(cx, "tab space");
        assert_eq!(clipboard(cx).as_deref(), Some("pie\n\"A\" : 1\n"));
        keys(cx, "shift-tab space");
        assert!(cx.debug_bounds("reply-0-diagram-4-source").is_none());
        keys(cx, "tab ctrl-shift-c");
        assert_eq!(clipboard(cx).as_deref(), Some("pie\n\"A\" : 1\n"));
        view.read_with(cx, |v, cx| {
            assert!(!v.state.read(cx).conversations()[&1].running);
            assert_eq!(v.state.read(cx).conversations()[&1].rows.len(), 1);
        });
    }

    #[gpui::test]
    fn transcript_keyboard_streaming_diagram_keeps_copy_identity_when_closed(
        cx: &mut gpui::TestAppContext,
    ) {
        fn keys(cx: &mut gpui::VisualTestContext, strokes: &str) {
            for key in strokes.split_whitespace() {
                cx.simulate_keystrokes(key);
                cx.simulate_event(gpui::KeyUpEvent {
                    keystroke: gpui::Keystroke::parse(key).unwrap(),
                });
                cx.run_until_parked();
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = open_with_session(dir.path(), cx);
        wait_until(cx, &view, |v, cx| {
            !v.state.read(cx).conversations()[&1].is_loading()
        });
        let source = "flowchart TD\n  A[é] --> B  \n";
        view.update(cx, |v, cx| {
            v.state.update(cx, |state, _| {
                state.test_conversation(1).apply(Event::ItemStarted(Item {
                    id: "stream-diagram".into(),
                    kind: ItemKind::AgentMessage,
                    text: format!("```mermaid\n{source}"),
                    status: ItemStatus::InProgress,
                }))
            });
            v.commit(1, cx);
            cx.notify();
        });
        let clipboard = |cx: &mut gpui::VisualTestContext| {
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()))
        };
        keys(cx, "ctrl-shift-t end tab enter");
        assert_eq!(clipboard(cx).as_deref(), Some(source));
        assert!(cx.debug_bounds("reply-0-diagram-1-toggle").is_none());
        view.update(cx, |v, cx| {
            v.state.update(cx, |state, _| {
                state.test_conversation(1).apply(Event::ItemCompleted(Item {
                    id: "stream-diagram".into(),
                    kind: ItemKind::AgentMessage,
                    text: format!("```mermaid\n{source}```\n\n```text\nafter\n```\n"),
                    status: ItemStatus::Completed,
                }))
            });
            v.commit(1, cx);
            cx.notify();
        });
        keys(cx, "space ctrl-shift-c");
        assert_eq!(clipboard(cx).as_deref(), Some(source));
        keys(cx, "tab enter");
        assert_eq!(clipboard(cx).as_deref(), Some("after"));
        keys(cx, "shift-tab enter");
        assert_eq!(clipboard(cx).as_deref(), Some(source));
    }

    #[gpui::test]
    fn transcript_keyboard_diagrams_do_not_expose_untracked_tab_stops(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = open_with_session(dir.path(), cx);
        wait_until(cx, &view, |v, cx| {
            !v.state.read(cx).conversations()[&1].is_loading()
        });
        view.update(cx, |v, cx| {
            v.state.update(cx, |state, _| {
                state.test_conversation(1).apply(Event::ItemCompleted(Item {
                    id: "diagram-tab-stops".into(),
                    kind: ItemKind::AgentMessage,
                    text: "intro\n\n```mermaid\nflowchart TD\nA-->B\n```\n".into(),
                    status: ItemStatus::Completed,
                }))
            });
            v.commit(1, cx);
            cx.notify();
        });
        for from_composer in [false, true] {
            view.update_in(cx, |v, window, cx| {
                if from_composer {
                    v.focus_composer(window, cx);
                } else {
                    v.focus.focus(window, cx);
                }
            });
            for step in 0..64 {
                cx.simulate_keystrokes(if from_composer { "tab" } else { "shift-tab" });
                cx.run_until_parked();
                view.update_in(cx, |v, window, cx| {
                    let views = &mut v.presentation.current.views;
                    assert!(
                        !views.contains_focus(MAIN_1, window, cx)
                            || views.focused_region(MAIN_1, window)
                            || views.keyboard_state(MAIN_1).is_some_and(|k| {
                                k.action_focus.is_focused(window)
                                    || k.jump_focus.is_focused(window)
                            }),
                        "generic Tab entered a diagram outside the transcript focus model at step {step}"
                    );
                });
            }
        }
    }

    #[gpui::test]
    fn transcript_escape_dismisses_customize_and_menus_before_leaving(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = open_with_session(dir.path(), cx);
        cx.simulate_keystrokes("ctrl-shift-l f6 escape");
        view.read_with(cx, |v, _cx| {
            assert!(!v.presentation.current.customization.is_open())
        });
        click(cx, "filter");
        cx.simulate_keystrokes("f6 escape");
        view.read_with(cx, |v, _cx| assert!(!v.filter_menu));
        cx.simulate_keystrokes("escape");
        cx.simulate_input("plain Escape leaves after dismissing overlays");
        view.read_with(cx, |v, cx| {
            assert_eq!(
                v.composer.read(cx).value(),
                "plain Escape leaves after dismissing overlays"
            )
        });
    }

    #[gpui::test]
    fn transcript_keyboard_copy_after_pointer_focus_and_escape(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        seed_long_session(&dir.path().join("sessions"), 1, 40);
        let (view, cx) = open_with_session(dir.path(), cx);
        wait_until(cx, &view, |v, cx| {
            v.state.read(cx).conversations()[&1].rows.len() == 120
        });
        cx.simulate_keystrokes("ctrl-shift-t home");
        click(cx, "conversation");
        cx.simulate_keystrokes("ctrl-shift-c");
        let text = cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()));
        assert_eq!(text.as_deref(), Some("Question 0"));
        cx.simulate_keystrokes("escape");
        cx.simulate_input("exit kept the draft");
        view.read_with(cx, |v, cx| {
            assert_eq!(v.composer.read(cx).value(), "exit kept the draft")
        });
        cx.simulate_keystrokes("ctrl-shift-t home");
        let over = cx.debug_bounds("conversation").unwrap().center();
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: over,
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-150.))),
            ..Default::default()
        });
        cx.run_until_parked();
        let top = view.read_with(cx, |v, _cx| keyboard_scroll_top(v).0);
        cx.simulate_keystrokes("down");
        view.read_with(cx, |v, _cx| {
            assert_eq!(
                v.presentation
                    .current
                    .views
                    .keyboard_state(MAIN_1)
                    .unwrap()
                    .row,
                Some(top)
            )
        });
    }

    #[gpui::test]
    fn transcript_keyboard_reveals_a_partially_clipped_destination(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        seed_long_session(&dir.path().join("sessions"), 1, 10);
        let (view, cx) = open_with_session(dir.path(), cx);
        wait_until(cx, &view, |v, cx| {
            v.state.read(cx).conversations()[&1].rows.len() == 30
        });
        cx.simulate_resize(size(px(1000.), px(500.)));
        cx.simulate_keystrokes("ctrl-shift-t home down");
        let top = view.read_with(cx, |v, _cx| keyboard_scroll_top(v));
        cx.simulate_keystrokes("down");
        view.read_with(cx, |v, _cx| assert_ne!(keyboard_scroll_top(v), top));
    }

    #[gpui::test]
    fn transcript_keyboard_entry_preserves_latest_and_visible_row_offsets(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        seed_long_session(&dir.path().join("sessions"), 1, 10);
        let (view, cx) = open_with_session(dir.path(), cx);
        wait_until(cx, &view, |v, cx| {
            v.state.read(cx).conversations()[&1].rows.len() == 30
        });
        view.update(cx, |v, cx| {
            v.state.update(cx, |state, _| {
                state.test_conversation(1).apply(Event::ItemCompleted(Item {
                    id: "tall".into(),
                    kind: ItemKind::AgentMessage,
                    text: (0..120).map(|n| format!("Paragraph {n}.\n\n")).collect(),
                    status: ItemStatus::Completed,
                }))
            });
            v.commit(1, cx);
            cx.notify();
        });
        cx.run_until_parked();
        let top = view.read_with(cx, |v, _cx| keyboard_scroll_top(v));
        cx.simulate_keystrokes("ctrl-shift-t");
        view.read_with(cx, |v, _cx| assert_eq!(keyboard_scroll_top(v), top));
        assert!(cx.debug_bounds("jump-to-latest").is_none());
        cx.simulate_keystrokes("home");
        let top = view.read_with(cx, |v, _cx| keyboard_scroll_top(v));
        cx.simulate_keystrokes("down");
        view.read_with(cx, |v, _cx| assert_eq!(keyboard_scroll_top(v), top));
    }

    #[gpui::test]
    fn transcript_keyboard_reentry_after_send_stays_at_latest(cx: &mut gpui::TestAppContext) {
        let _env = codex_env();
        let dir = tempfile::tempdir().unwrap();
        seed_long_session(&dir.path().join("sessions"), 1, 40);
        let (view, cx) = open_with_session(dir.path(), cx);
        wait_until(cx, &view, |v, cx| {
            v.state.read(cx).conversations()[&1].rows.len() == 120
        });
        cx.simulate_keystrokes("ctrl-shift-t home down ctrl-l");
        send_text(&view, cx, "hello");
        wait_until(cx, &view, |v, cx| {
            !v.state.read(cx).conversations()[&1].running
                && reply_with(v, "You wrote: hello", cx).is_some()
        });
        let top = view.read_with(cx, |v, _cx| keyboard_scroll_top(v));
        cx.simulate_keystrokes("ctrl-shift-t");
        view.read_with(cx, |v, _cx| {
            assert!(!v.presentation.current.views.scrolled_up(MAIN_1));
            assert_eq!(keyboard_scroll_top(v), top);
        });
    }

    #[gpui::test]
    fn transcript_keyboard_replaces_a_streamed_row_that_becomes_hidden(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = open_with_session(dir.path(), cx);
        wait_until(cx, &view, |v, cx| {
            !v.state.read(cx).conversations()[&1].is_loading()
        });
        view.update(cx, |v, cx| {
            v.state.update(cx, |state, _| {
                state.test_conversation(1).push_local("visible user row")
            });
            v.state.update(cx, |state, _| {
                state.test_conversation(1).apply(Event::ItemStarted(Item {
                    id: "reasoning".into(),
                    kind: ItemKind::Reasoning,
                    text: "Thinking".into(),
                    status: ItemStatus::InProgress,
                }))
            });
            v.commit(1, cx);
            cx.notify();
        });
        cx.simulate_keystrokes("ctrl-shift-t end");
        assert!(cx.debug_bounds("transcript-keyboard-row-1").is_some());
        view.update(cx, |v, cx| {
            v.state.update(cx, |state, _| {
                state.test_conversation(1).apply(Event::ItemCompleted(Item {
                    id: "reasoning".into(),
                    kind: ItemKind::Reasoning,
                    text: String::new(),
                    status: ItemStatus::Completed,
                }))
            });
            v.commit(1, cx);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("transcript-keyboard-row-0").is_some());
        cx.simulate_keystrokes("ctrl-shift-t ctrl-shift-c");
        let text = cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()));
        assert_eq!(text.as_deref(), Some("visible user row"));
    }

    #[gpui::test]
    fn welcome_action_focuses_composer_and_preserves_draft_on_session_switch(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let store = Store::open(&path).unwrap();
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        assert!(cx.debug_bounds("welcome-heading").is_some());
        assert!(cx.debug_bounds("welcome-codex").is_some());

        let bounds = cx.debug_bounds("welcome-claude").unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(Store::open(&path).unwrap().workspace().selected(), Some(1));
        cx.simulate_input("Draft from the welcome composer\n第二行 😀\n");
        cx.run_until_parked();

        for selector in ["new-session", "new-codex", "session-1"] {
            let bounds = cx.debug_bounds(selector).expect("session control rendered");
            cx.simulate_click(bounds.center(), gpui::Modifiers::none());
            cx.run_until_parked();
        }
        view.read_with(cx, |view, cx| {
            assert_eq!(view.state.read(cx).store().workspace().selected(), Some(1));
            assert_eq!(
                view.composer.read(cx).value(),
                "Draft from the welcome composer\n第二行 😀\n"
            );
        });
        assert_eq!(
            Store::open(&path).unwrap().workspace().sessions()[0].draft(),
            "Draft from the welcome composer\n第二行 😀\n"
        );
    }

    #[gpui::test]
    #[ignore = "benchmark; run through scripts/perf.py"]
    fn perf_sidebar_draw(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        for index in 0..200 {
            let provider = if index % 2 == 0 {
                Provider::Claude
            } else {
                Provider::Codex
            };
            store.create(provider, &working_directory(), 1).unwrap();
        }
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        // A composer keystroke notifies only the root view. A session change also
        // invalidates the cached sidebar, so it rebuilds every row.
        for (metric, sidebar_changed) in [("sidebar_draw_ms", false), ("sidebar_refresh_ms", true)]
        {
            let mut total = std::time::Duration::ZERO;
            let frames = 30;
            for _ in 0..frames {
                view.update(cx, |this, cx| {
                    if sidebar_changed {
                        this.refresh(cx);
                    } else {
                        cx.notify();
                    }
                });
                cx.update(|window, cx| {
                    let start = std::time::Instant::now();
                    window.draw(cx).clear(cx);
                    total += start.elapsed();
                });
            }
            eprintln!(
                "PERF {metric}={:.2}",
                total.as_secs_f64() * 1000. / frames as f64
            );
        }
    }

    /// Writes a session log with `turns` completed turns: a question, a command, and a
    /// several-line reply each.
    fn seed_long_session(dir: &Path, session: u64, turns: usize) {
        use keystone_agent::log::{Entry, SessionLog};
        let mut log = SessionLog::open(dir, session).unwrap();
        for turn in 0..turns {
            let item = |id: &str, kind: ItemKind, text: String| Entry::Item {
                item: Item {
                    id: format!("{id}-{turn}"),
                    kind,
                    text,
                    status: ItemStatus::Completed,
                },
                seq: None,
                run: None,
            };
            log.append(&Entry::TurnStarted).unwrap();
            log.append(&item(
                "u",
                ItemKind::UserMessage,
                format!("Question {turn}"),
            ))
            .unwrap();
            log.append(&item(
                "c",
                ItemKind::Command {
                    command: "cargo test".into(),
                    exit_code: Some(0),
                },
                "running 12 tests\ntest result: ok".into(),
            ))
            .unwrap();
            log.append(&item(
                "a",
                ItemKind::AgentMessage,
                format!("Reply {turn}. ").repeat(40),
            ))
            .unwrap();
            log.append(&Entry::TurnEnded {
                status: TurnStatus::Completed,
                error: None,
                seq: None,
                run: None,
            })
            .unwrap();
        }
        log.sync().unwrap();
    }

    #[gpui::test]
    #[ignore = "benchmark; run through scripts/perf.py"]
    fn perf_transcript_draw(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let mut store = Store::open(&path).unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1)
            .unwrap();
        seed_long_session(&dir.path().join("sessions"), 1, 1000);
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        wait_until(cx, &view, |view, cx| {
            view.state
                .read(cx)
                .conversations()
                .get(&1)
                .is_some_and(|c| c.rows.len() == 3000)
        });
        // A composer keystroke notifies the root view, which rebuilds the transcript.
        let mut total = std::time::Duration::ZERO;
        let frames = 30;
        for _ in 0..frames {
            view.update(cx, |_, cx| cx.notify());
            cx.update(|window, cx| {
                let start = std::time::Instant::now();
                window.draw(cx).clear(cx);
                total += start.elapsed();
            });
        }
        eprintln!(
            "PERF transcript_draw_ms={:.2}",
            total.as_secs_f64() * 1000. / frames as f64
        );

        view.update_in(cx, |this, window, cx| {
            this.transcript_row(MAIN_1, 2, window, cx);
            this.transcript_action(MAIN_1, false, window, cx);
        });
        let mut total = std::time::Duration::ZERO;
        for _ in 0..frames {
            view.update(cx, |_, cx| cx.notify());
            cx.update(|window, cx| {
                let start = std::time::Instant::now();
                window.draw(cx).clear(cx);
                total += start.elapsed();
            });
        }
        eprintln!(
            "PERF transcript_action_draw_ms={:.2}",
            total.as_secs_f64() * 1000. / frames as f64
        );

        // A reply streaming at the end of the same transcript: one delta per frame.
        view.update(cx, |this, cx| {
            this.state.update(cx, |state, _| {
                let conversation = state.test_conversation(1);
                conversation.apply(Event::TurnStarted);
                conversation.apply(Event::ItemStarted(Item {
                    id: "live".into(),
                    kind: ItemKind::AgentMessage,
                    text: String::new(),
                    status: ItemStatus::InProgress,
                }));
            });
            this.commit(1, cx);
            cx.notify();
        });
        let mut total = std::time::Duration::ZERO;
        for _ in 0..frames {
            view.update(cx, |this, cx| {
                this.state.update(cx, |state, _| {
                    state.test_conversation(1).apply(Event::ItemDelta {
                        item_id: "live".into(),
                        text: "More streamed text. ".into(),
                    })
                });
                this.commit(1, cx);
                cx.notify();
            });
            cx.update(|window, cx| {
                let start = std::time::Instant::now();
                window.draw(cx).clear(cx);
                total += start.elapsed();
            });
        }
        eprintln!(
            "PERF transcript_stream_ms={:.2}",
            total.as_secs_f64() * 1000. / frames as f64
        );
    }

    #[gpui::test]
    fn a_long_transcript_draws_only_rows_in_view_and_follows_new_rows(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1)
            .unwrap();
        seed_long_session(&dir.path().join("sessions"), 1, 200);
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        wait_until(cx, &view, |view, cx| {
            view.state
                .read(cx)
                .conversations()
                .get(&1)
                .is_some_and(|c| c.rows.len() == 600)
        });
        cx.run_until_parked();
        // Opened at the latest row. Rows far above were never laid out.
        assert!(cx.debug_bounds("transcript-row-599").is_some());
        assert!(cx.debug_bounds("transcript-row-0").is_none());
        assert!(cx.debug_bounds("transcript-row-300").is_none());

        // A row that arrives while at the bottom is followed.
        let reply = |id: &str| {
            Event::ItemCompleted(Item {
                id: id.into(),
                kind: ItemKind::AgentMessage,
                text: format!("Reply {id}"),
                status: ItemStatus::Completed,
            })
        };
        view.update(cx, |this, cx| {
            this.state.update(cx, |state, _| {
                state.test_conversation(1).apply(reply("new"))
            });
            this.commit(1, cx);
            cx.notify();
        });
        cx.run_until_parked();
        let composer = cx.debug_bounds("composer-field").unwrap();
        let newest = cx
            .debug_bounds("transcript-row-600")
            .expect("new row drawn");
        assert!(newest.bottom() <= composer.top(), "the new row is in view");

        // The mouse wheel scrolls up through the history.
        let last = cx.debug_bounds("transcript-row-600").unwrap();
        let over = cx.debug_bounds("conversation").unwrap().center();
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: over,
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(30.))),
            ..Default::default()
        });
        cx.run_until_parked();
        let moved = cx.debug_bounds("transcript-row-600").unwrap();
        assert_eq!(moved.top() - last.top(), px(30.), "scrolled by the wheel");

        // Scrolled to the top, the first rows draw and a new row does not pull the
        // view back down.
        view.update(cx, |this, cx| {
            this.presentation
                .current
                .views
                .get(MAIN_1)
                .unwrap()
                .scroll_to(ListOffset::default());
            cx.notify();
        });
        cx.run_until_parked();
        let first = cx
            .debug_bounds("transcript-row-0")
            .expect("first row drawn");
        view.update(cx, |this, cx| {
            this.state.update(cx, |state, _| {
                state.test_conversation(1).apply(reply("later"))
            });
            this.commit(1, cx);
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(cx.debug_bounds("transcript-row-0").unwrap(), first);
        assert!(cx.debug_bounds("transcript-row-601").is_none());
        view.update(cx, |this, cx| {
            this.presentation.current.views.scroll_to_end(MAIN_1);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("transcript-row-601").is_some());
    }

    #[gpui::test]
    fn streaming_keeps_the_place_of_a_reader_who_scrolled_up(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1)
            .unwrap();
        seed_long_session(&dir.path().join("sessions"), 1, 200);
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        wait_until(cx, &view, |view, cx| {
            view.state
                .read(cx)
                .conversations()
                .get(&1)
                .is_some_and(|c| c.rows.len() == 600)
        });
        let message = |id: &str, text: String| Item {
            id: id.into(),
            kind: ItemKind::AgentMessage,
            text,
            status: ItemStatus::InProgress,
        };
        // A short item, then a reply taller than the view, both still streaming.
        view.update(cx, |this, cx| {
            this.state.update(cx, |state, _| {
                let conversation = state.test_conversation(1);
                conversation.apply(Event::TurnStarted);
                conversation.apply(Event::ItemStarted(message("early", "Thinking".into())));
                conversation.apply(Event::ItemStarted(message(
                    "long",
                    "Streamed words. ".repeat(600),
                )));
            });
            this.commit(1, cx);
            cx.notify();
        });
        cx.run_until_parked();
        // Scroll up partway into the long reply.
        let over = cx.debug_bounds("conversation").unwrap().center();
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: over,
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(200.))),
            ..Default::default()
        });
        cx.run_until_parked();
        let view_top = cx.debug_bounds("conversation").unwrap().top();
        let read = cx.debug_bounds("transcript-row-601").unwrap();
        assert!(read.top() < view_top, "the reply starts above the view");
        assert!(cx.debug_bounds("transcript-row-600").is_none());

        // More of the reply streams in: the text being read stays where it is.
        view.update(cx, |this, cx| {
            this.state.update(cx, |state, _| {
                state.test_conversation(1).apply(Event::ItemDelta {
                    item_id: "long".into(),
                    text: "More. ".repeat(50),
                })
            });
            this.commit(1, cx);
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            cx.debug_bounds("transcript-row-601").unwrap().top(),
            read.top()
        );

        // An item above the view completes: still no jump.
        view.update(cx, |this, cx| {
            this.state.update(cx, |state, _| {
                state.test_conversation(1).apply(Event::ItemCompleted(Item {
                    status: ItemStatus::Completed,
                    ..message("early", "Thought for a while. ".repeat(20))
                }))
            });
            this.commit(1, cx);
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            cx.debug_bounds("transcript-row-601").unwrap().top(),
            read.top()
        );
        assert!(cx.debug_bounds("transcript-row-600").is_none());
    }

    #[gpui::test]
    fn each_view_of_a_conversation_keeps_its_own_place_while_it_streams(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1)
            .unwrap();
        store
            .create(Provider::Codex, &working_directory(), 2)
            .unwrap();
        store.select(1).unwrap();
        seed_long_session(&dir.path().join("sessions"), 1, 200);
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        wait_until(cx, &view, |view, cx| {
            view.state
                .read(cx)
                .conversations()
                .get(&1)
                .is_some_and(|c| c.rows.len() == 600)
        });
        cx.run_until_parked();
        let other = ViewKey {
            view: ViewId(1),
            session: 1,
        };
        let reader = (100, px(40.));
        view.update(cx, |this, _cx| {
            this.presentation
                .current
                .views
                .list(other, 600)
                .scroll_to(at(100, 40.));
        });
        let stream = |view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext, id: &str| {
            view.update(cx, |this, cx| {
                this.state.update(cx, |state, _| {
                    state.test_conversation(1).apply(Event::ItemCompleted(Item {
                        id: id.into(),
                        kind: ItemKind::AgentMessage,
                        text: format!("Reply {id}"),
                        status: ItemStatus::Completed,
                    }))
                });
                this.commit(1, cx);
                cx.notify();
            });
            cx.run_until_parked();
        };
        let tops = |view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext| {
            view.update(cx, |this, _cx| {
                let main = this.presentation.current.views.get(MAIN_1).unwrap();
                let other = this.presentation.current.views.get(other).unwrap();
                assert_eq!(main.item_count(), other.item_count());
                let top = other.logical_scroll_top();
                (
                    main.logical_scroll_top().item_ix == main.item_count(),
                    (top.item_ix, top.offset_in_item),
                )
            })
        };

        stream(&view, cx, "a");
        assert!(cx.debug_bounds("transcript-row-600").is_some());
        assert_eq!(tops(&view, cx), (true, reader));

        let row = cx.debug_bounds("session-2").unwrap().center();
        cx.simulate_click(row, gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(cx.debug_bounds("transcript-row-600").is_none());
        stream(&view, cx, "b");
        assert_eq!(tops(&view, cx), (true, reader));

        let row = cx.debug_bounds("session-1").unwrap().center();
        cx.simulate_click(row, gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(cx.debug_bounds("transcript-row-601").is_some());
        assert_eq!(tops(&view, cx), (true, reader));
        view.update(cx, |this, _cx| {
            assert_eq!(
                this.presentation
                    .current
                    .views
                    .get(other)
                    .unwrap()
                    .item_count(),
                603
            );
            assert!(!this.presentation.current.views.take_remeasured_top(other));
            this.presentation
                .current
                .views
                .get(other)
                .unwrap()
                .scroll_to(at(600, 40.));
        });

        view.update(cx, |this, cx| {
            this.state.update(cx, |state, _| {
                state.test_conversation(1).apply(Event::ItemCompleted(Item {
                    id: "a".into(),
                    kind: ItemKind::AgentMessage,
                    text: "Short".into(),
                    status: ItemStatus::Completed,
                }))
            });
            this.commit(1, cx);
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(tops(&view, cx), (true, (600, px(40.))));
        view.update(cx, |this, _cx| {
            assert_eq!(
                this.presentation
                    .current
                    .views
                    .get(other)
                    .unwrap()
                    .item_count(),
                603
            );
            assert!(this.presentation.current.views.take_remeasured_top(other));
            assert!(!this.presentation.current.views.take_remeasured_top(MAIN_1));
        });
    }

    #[gpui::test]
    fn a_streamed_burst_reveals_over_frames_while_its_turn_state_shows_at_once(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx, _dir) = long_session(cx);
        cx.run_until_parked();
        let receive = |view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext, events| {
            view.update(cx, |this, cx| this.receive(Vec::new(), events, cx));
            cx.run_until_parked();
        };
        let height = |cx: &mut gpui::VisualTestContext| {
            cx.debug_bounds("transcript-row-600").unwrap().size.height
        };
        let tick = |cx: &mut gpui::VisualTestContext| {
            cx.executor().advance_clock(reveal::TICK);
            cx.run_until_parked();
        };
        let reply = "Streamed words. ".repeat(100);
        receive(
            &view,
            cx,
            vec![
                (1, Event::TurnStarted),
                (
                    1,
                    Event::ItemStarted(Item {
                        id: "reply".into(),
                        kind: ItemKind::AgentMessage,
                        text: String::new(),
                        status: ItemStatus::InProgress,
                    }),
                ),
                (
                    1,
                    Event::ItemDelta {
                        item_id: "reply".into(),
                        text: reply.clone(),
                    },
                ),
                (
                    1,
                    Event::ApprovalRequested(keystone_agent::Approval {
                        request: keystone_agent::RequestId("1".into()),
                        item_id: None,
                        action: "cargo test".into(),
                        detail: None,
                    }),
                ),
            ],
        );
        assert!(cx.debug_bounds("approve").is_some());
        view.read_with(cx, |this, cx| {
            assert!(
                matches!(&this.state.read(cx).conversations()[&1].rows[600], Row::Item(item) if item.text == reply)
            );
        });
        let first = height(cx);
        tick(cx);
        let second = height(cx);
        assert!(second > first, "the reply grows on the next frame");
        for _ in 0..10 {
            tick(cx);
        }
        let full = height(cx);
        assert!(full > second);
        view.read_with(cx, |this, _cx| {
            assert!(this.reveal_task.is_none(), "no timer once caught up")
        });

        let over = cx.debug_bounds("conversation").unwrap().center();
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: over,
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(120.))),
            ..Default::default()
        });
        cx.run_until_parked();
        let read = cx.debug_bounds("transcript-row-600").unwrap().top();
        receive(
            &view,
            cx,
            vec![(
                1,
                Event::ItemDelta {
                    item_id: "reply".into(),
                    text: "More words. ".repeat(100),
                },
            )],
        );
        for _ in 0..3 {
            tick(cx);
            assert_eq!(cx.debug_bounds("transcript-row-600").unwrap().top(), read);
        }

        receive(
            &view,
            cx,
            vec![(
                1,
                Event::TurnCompleted {
                    status: TurnStatus::Completed,
                    error: None,
                    stopped: false,
                },
            )],
        );
        let done = height(cx);
        tick(cx);
        assert_eq!(height(cx), done, "nothing left to reveal");

        click(cx, "jump-to-latest");
        assert!(cx.debug_bounds("jump-to-latest").is_none());
        let composer = cx.debug_bounds("composer-field").unwrap();
        let row = cx.debug_bounds("transcript-row-600").unwrap();
        assert!(row.bottom() <= composer.top());
        assert!(cx.debug_bounds("copy-reply-600").is_some());
    }

    #[gpui::test]
    fn a_session_selected_again_shows_its_backlog_at_once(cx: &mut gpui::TestAppContext) {
        let (view, cx, _dir) = long_session(cx);
        let delta = |text: &str| {
            (
                1,
                Event::ItemDelta {
                    item_id: "reply".into(),
                    text: text.into(),
                },
            )
        };
        view.update(cx, |this, cx| {
            let reply = Item {
                id: "reply".into(),
                kind: ItemKind::AgentMessage,
                text: "Shown. ".into(),
                status: ItemStatus::InProgress,
            };
            let events = vec![(1, Event::TurnStarted), (1, Event::ItemStarted(reply))];
            this.receive(Vec::new(), events, cx);
        });
        cx.run_until_parked();
        view.update(cx, |this, cx| {
            this.receive(Vec::new(), vec![delta(&"Visible burst. ".repeat(100))], cx);
        });
        view.update(cx, |this, cx| {
            assert!(this.replies.get_mut().pacing());
            this.create(Provider::Codex, None, cx);
            assert!(
                !this.replies.get_mut().pacing(),
                "new sessions settle the reply being left"
            );
        });
        cx.run_until_parked();

        view.update(cx, |this, cx| {
            this.receive(Vec::new(), vec![delta(&"Backlog. ".repeat(100))], cx);
            this.select(1, cx);
            this.receive(Vec::new(), vec![delta("More. ")], cx);
            assert!(
                !this.replies.get_mut().pacing(),
                "the backlog is not replayed"
            );
        });
    }

    /// Opens a long session and returns its view once the history is shown.
    fn long_session(
        cx: &mut gpui::TestAppContext,
    ) -> (
        Entity<Keystone>,
        &mut gpui::VisualTestContext,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1)
            .unwrap();
        seed_long_session(&dir.path().join("sessions"), 1, 200);
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        wait_until(cx, &view, |view, cx| {
            view.state
                .read(cx)
                .conversations()
                .get(&1)
                .is_some_and(|c| c.rows.len() == 600)
        });
        (view, cx, dir)
    }

    fn at(item_ix: usize, offset: f32) -> ListOffset {
        ListOffset {
            item_ix,
            offset_in_item: px(offset),
        }
    }

    fn scroll_to(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext, top: ListOffset) {
        view.update(cx, |this, cx| {
            this.presentation
                .current
                .views
                .get(MAIN_1)
                .unwrap()
                .scroll_to(top);
            cx.notify();
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn a_reader_scrolled_into_a_row_that_shrinks_sees_the_next_row(cx: &mut gpui::TestAppContext) {
        let (view, cx, _dir) = long_session(cx);
        let item = |id: &str, kind: ItemKind, text: String| Item {
            id: id.into(),
            kind,
            text,
            status: ItemStatus::InProgress,
        };
        let long = || "Streamed words. ".repeat(600);
        let update = |view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext, event| {
            view.update(cx, |this, cx| {
                this.state
                    .update(cx, |state, _| state.test_conversation(1).apply(event));
                this.commit(1, cx);
                cx.notify();
            });
            cx.run_until_parked();
        };
        view.update(cx, |this, cx| {
            this.state.update(cx, |state, _| {
                let conversation = state.test_conversation(1);
                conversation.apply(Event::TurnStarted);
                conversation.apply(Event::ItemStarted(item(
                    "a",
                    ItemKind::AgentMessage,
                    long(),
                )));
                conversation.apply(Event::ItemStarted(item("r", ItemKind::Reasoning, long())));
                conversation.apply(Event::ItemStarted(item(
                    "b",
                    ItemKind::AgentMessage,
                    long(),
                )));
            });
            this.commit(1, cx);
            cx.notify();
        });
        cx.run_until_parked();
        scroll_to(&view, cx, at(600, 0.));
        let content_top = cx.debug_bounds("transcript-row-600").unwrap().top();

        // Reading 1000px into a reply that is then replaced by a short one.
        scroll_to(&view, cx, at(600, 1000.));
        update(
            &view,
            cx,
            Event::ItemCompleted(Item {
                status: ItemStatus::Completed,
                ..item("a", ItemKind::AgentMessage, "Short.".into())
            }),
        );
        assert_eq!(
            cx.debug_bounds("transcript-row-601").unwrap().top(),
            content_top,
            "the next row starts at the top, not above it"
        );

        // Reading inside a reasoning summary that completes empty, so it is not shown.
        scroll_to(&view, cx, at(601, 500.));
        update(
            &view,
            cx,
            Event::ItemCompleted(Item {
                status: ItemStatus::Completed,
                ..item("r", ItemKind::Reasoning, String::new())
            }),
        );
        assert_eq!(
            cx.debug_bounds("transcript-row-602").unwrap().top(),
            content_top
        );
    }

    /// Opens a long session in a window of the given width, with three long replies
    /// streaming at its end, scrolled to the start of the first.
    fn long_replies(
        cx: &mut gpui::TestAppContext,
        width: f32,
    ) -> (
        Entity<Keystone>,
        &mut gpui::VisualTestContext,
        tempfile::TempDir,
        Pixels,
    ) {
        let (view, cx, dir) = long_session(cx);
        cx.simulate_resize(size(px(width), px(700.)));
        cx.run_until_parked();
        view.update(cx, |this, cx| {
            this.state.update(cx, |state, _| {
                let conversation = state.test_conversation(1);
                conversation.apply(Event::TurnStarted);
                for id in ["a", "b", "c"] {
                    conversation.apply(Event::ItemStarted(Item {
                        id: id.into(),
                        kind: ItemKind::AgentMessage,
                        text: "Streamed words. ".repeat(600),
                        status: ItemStatus::InProgress,
                    }));
                }
            });
            this.commit(1, cx);
            cx.notify();
        });
        cx.run_until_parked();
        scroll_to(&view, cx, at(600, 0.));
        let content_top = cx.debug_bounds("transcript-row-600").unwrap().top();
        (view, cx, dir, content_top)
    }

    #[gpui::test]
    fn a_reader_scrolled_into_a_row_that_a_wider_window_shortens_sees_the_next_row(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx, _dir, content_top) = long_replies(cx, 600.);
        let narrow = cx.debug_bounds("transcript-row-600").unwrap().size.height;
        let offset = narrow * 0.95;
        scroll_to(&view, cx, at(600, offset.into()));
        let view_top = cx.debug_bounds("conversation").unwrap().top();

        // The reply wraps onto fewer lines, ending before the place being read.
        cx.simulate_resize(size(px(1500.), px(700.)));
        cx.run_until_parked();
        let moved = cx.debug_bounds("conversation").unwrap().top() - view_top;
        let next = cx.debug_bounds("transcript-row-601").unwrap();
        assert!(
            next.size.height < offset,
            "the replies are as long as each other"
        );
        assert_eq!(
            next.top(),
            content_top + moved,
            "the next row starts at the top, not above it"
        );

        // Following the bottom is unchanged by a resize.
        view.update(cx, |this, cx| {
            this.presentation.current.views.scroll_to_end(MAIN_1);
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_resize(size(px(900.), px(700.)));
        cx.run_until_parked();
        view.update(cx, |this, _cx| {
            let list = this.presentation.current.views.get(MAIN_1).unwrap();
            assert_eq!(list.logical_scroll_top().item_ix, list.item_count());
        });
    }

    #[gpui::test]
    fn a_reader_scrolled_into_a_row_that_a_narrower_window_lengthens_keeps_their_place(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx, _dir, content_top) = long_replies(cx, 1500.);
        scroll_to(&view, cx, at(600, 1000.));
        let wide = cx.debug_bounds("transcript-row-600").unwrap().size.height;
        assert!(wide > px(1000.), "the place being read is inside the reply");
        let view_top = cx.debug_bounds("conversation").unwrap().top();

        // The reply wraps onto more lines; the place being read is still inside it.
        cx.simulate_resize(size(px(600.), px(700.)));
        cx.run_until_parked();
        let moved = cx.debug_bounds("conversation").unwrap().top() - view_top;
        let row = cx.debug_bounds("transcript-row-600").unwrap();
        assert!(row.size.height > wide);
        assert_eq!(row.top(), content_top + moved - px(1000.));
    }

    #[gpui::test]
    fn a_reader_scrolled_into_the_status_keeps_their_place(cx: &mut gpui::TestAppContext) {
        let (view, cx, _dir) = long_session(cx);
        // A tall approval card in the status entry.
        view.update(cx, |this, cx| {
            this.state.update(cx, |state, _| {
                let conversation = state.test_conversation(1);
                conversation.apply(Event::TurnStarted);
                conversation.apply(Event::ApprovalRequested(keystone_agent::Approval {
                    request: keystone_agent::RequestId("1".into()),
                    item_id: None,
                    action: "cargo test".into(),
                    detail: Some("A long reason to run it. ".repeat(400)),
                }));
            });
            this.commit(1, cx);
            cx.notify();
        });
        cx.run_until_parked();
        scroll_to(&view, cx, at(600, 300.));
        let read = cx.debug_bounds("transcript-status").unwrap();
        let view_top = cx.debug_bounds("conversation").unwrap().top();
        assert!(read.top() < view_top, "scrolled into the card");

        // A row arrives before the status entry: the card stays where it was read.
        view.update(cx, |this, cx| {
            this.state.update(cx, |state, _| {
                state.test_conversation(1).apply(Event::ItemStarted(Item {
                    id: "m".into(),
                    kind: ItemKind::AgentMessage,
                    text: "Starting.".into(),
                    status: ItemStatus::InProgress,
                }))
            });
            this.commit(1, cx);
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            cx.debug_bounds("transcript-status").unwrap().top(),
            read.top()
        );
        assert!(cx.debug_bounds("transcript-row-600").is_none());
    }

    #[gpui::test]
    fn a_message_sent_while_history_loads_is_saved_on_quit(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1)
            .unwrap();
        // Selected, so session 1's history is not loaded until it is used below.
        store
            .create(Provider::Codex, &working_directory(), 1)
            .unwrap();
        let sessions = dir.path().join("sessions");
        seed_long_session(&sessions, 1, 3);
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        // Sent and quit before the loaded history is applied.
        view.update(cx, |this, cx| {
            assert!(!this.state.read(cx).conversations().contains_key(&1));
            assert!(
                this.state
                    .update(cx, |state, _| state.test_conversation(1).is_loading())
            );
            this.state.update(cx, |state, _| {
                state.test_conversation(1).push_local("Sent while loading")
            });
            this.commit(1, cx);
            this.flush_logs(cx);
        });
        let entries = keystone_agent::log::read(&sessions, 1).unwrap();
        assert!(
            matches!(entries.last(), Some(keystone_agent::log::Entry::Item { item, .. })
                if item.text == "Sent while loading"),
            "the message was saved: {:?}",
            entries.last()
        );
    }

    #[gpui::test]
    fn immediate_close_snapshots_the_exact_dirty_composer_once(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let (view, cx) = open_window(&path, cx);
        view.update(cx, |this, cx| {
            this.create(Provider::Codex, None, cx);
            let before = this.state.read(cx).store().writes();
            this.composer.update(cx, |input, cx| {
                input.set_value("  Exact draft\nwith whitespace  ".into(), cx)
            });
            assert!(this.save_before_exit(cx));
            assert_eq!(this.state.read(cx).save_calls, 1);
            assert_eq!(this.state.read(cx).store().writes(), before + 1);
        });
        assert_eq!(
            Store::open(path).unwrap().workspace().sessions()[0].draft(),
            "  Exact draft\nwith whitespace  "
        );
    }

    #[gpui::test]
    fn closing_does_not_wait_for_a_history_read_that_never_returns(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1)
            .unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1)
            .unwrap();
        let sessions = dir.path().join("sessions");
        seed_long_session(&sessions, 1, 3);
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        // Session 1's history read never returns, as on a stalled network disk.
        view.update(cx, |this, cx| {
            let read: keystone_agent::log::Reader = Arc::new(|dir, session| {
                if session == 1 {
                    loop {
                        std::thread::park();
                    }
                }
                keystone_agent::log::read(dir, session)
            });
            this.state.update(cx, |state, _| {
                state.test_set_logs(Logs::with_reader(
                    sessions.clone(),
                    state.test_wake().clone(),
                    read,
                ));
            });
            this.state
                .update(cx, |state, _| state.test_conversations_mut().clear());
            this.state.update(cx, |state, _| {
                state.test_conversation(2);
            });
        });
        wait_until(cx, &view, |view, cx| {
            !view.state.read(cx).conversations()[&2].is_loading()
        });
        let started = std::time::Instant::now();
        view.update(cx, |this, cx| {
            this.state.update(cx, |state, _| {
                state.test_conversation(2).push_local("In another session")
            });
            this.commit(2, cx);
            assert!(
                this.state
                    .update(cx, |state, _| state.test_conversation(1).is_loading())
            );
            this.state.update(cx, |state, _| {
                state.test_conversation(1).push_local("Sent while loading")
            });
            this.commit(1, cx);
            this.flush_logs(cx);
        });
        assert!(
            started.elapsed() < LOAD_WAIT + std::time::Duration::from_secs(3),
            "closing waited {:?}",
            started.elapsed()
        );
        let texts = |session| {
            let mut restored = Conversation::loading();
            restored.restore(Ok(keystone_agent::log::read(&sessions, session).unwrap()));
            restored
                .rows
                .iter()
                .filter_map(|row| match row {
                    Row::Item(item) => Some(item.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(texts(2), ["In another session"]);
        // The held message was saved, and still restores after the history.
        let held = texts(1);
        assert_eq!(held.first().map(String::as_str), Some("Question 0"));
        assert_eq!(held.last().map(String::as_str), Some("Sent while loading"));
        assert_eq!(held.len(), 3 * 3 + 1);
    }

    #[gpui::test]
    fn a_save_failure_shows_in_the_conversation_until_saving_works(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1)
            .unwrap();
        let sessions = dir.path().join("sessions");
        let writer = lock_database(store.path());
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        let turn = |view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext| {
            view.update(cx, |this, cx| {
                this.state.update(cx, |state, _| {
                    let conversation = state.test_conversation(1);
                    conversation.push_local("hello");
                    conversation.apply(Event::TurnStarted);
                });
                this.commit(1, cx);
                cx.notify();
            });
        };
        wait_until(cx, &view, |view, cx| {
            !view.state.read(cx).conversations()[&1].is_loading()
        });
        turn(&view, cx);
        wait_until(cx, &view, |view, cx| {
            view.state.read(cx).conversations()[&1].save_error.is_some()
        });
        assert!(cx.debug_bounds("transcript-status").is_some());
        drop(writer);
        turn(&view, cx);
        wait_until(cx, &view, |view, cx| {
            view.state.read(cx).conversations()[&1].save_error.is_none()
        });
        view.update(cx, |view, cx| view.flush_logs(cx));
        assert!(
            !keystone_agent::log::read(&sessions, 1).unwrap().is_empty(),
            "written once saving works"
        );
    }

    #[gpui::test]
    fn final_log_failure_is_visible_and_retry_saves_without_another_send(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1)
            .unwrap();
        let sessions = dir.path().join("sessions");
        let writer = lock_database(store.path());
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        wait_until(cx, &view, |view, cx| {
            !view.state.read(cx).conversations()[&1].is_loading()
        });
        view.update(cx, |this, cx| {
            this.state.update(cx, |state, _| {
                state
                    .test_conversation(1)
                    .push_local("retained while storage fails")
            });
            this.commit(1, cx);
            this.flush_logs(cx);
            assert!(
                this.error
                    .as_ref()
                    .unwrap()
                    .contains("Could not save conversation logs")
            );
            assert!(this.state.read(cx).conversations()[&1].save_error.is_some());
        });
        assert!(cx.debug_bounds("transcript-status").is_some());
        drop(writer);
        view.update(cx, |this, cx| {
            this.flush_logs(cx);
            assert!(this.state.read(cx).conversations()[&1].save_error.is_none());
        });
        let entries = keystone_agent::log::read(&sessions, 1).unwrap();
        let texts = entries
            .iter()
            .filter_map(|entry| match entry {
                keystone_agent::log::Entry::Item { item, .. } => Some(item.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(texts, ["retained while storage fails"]);
    }

    #[gpui::test]
    fn cached_sidebar_follows_search_and_settlement(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store
            .create(Provider::Claude, &working_directory(), 1)
            .unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1)
            .unwrap();
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        // GPUI 0.2.2 keeps debug bounds for elements that are no longer drawn, so check
        // which row sits at the top of the list instead of which rows exist.
        let top = |cx: &mut gpui::VisualTestContext| cx.debug_bounds("session-1").unwrap().origin;
        let second =
            |cx: &mut gpui::VisualTestContext| cx.debug_bounds("session-2").unwrap().origin;
        let (first_row, second_row) = (top(cx), second(cx));
        assert!(second_row.y > first_row.y);

        // Typing in the composer reuses the cached sidebar.
        view.update_in(cx, |this, window, cx| {
            this.composer.focus_handle(cx).focus(window, cx)
        });
        cx.simulate_input("Draft text");
        cx.run_until_parked();
        assert_eq!(second(cx), second_row);

        view.update_in(cx, |this, window, cx| {
            this.search.focus_handle(cx).focus(window, cx)
        });
        cx.simulate_input("codex");
        cx.run_until_parked();
        assert_eq!(second(cx), first_row);

        cx.update(|_, cx| TextInput::bind_keys(cx));
        cx.simulate_keystrokes("ctrl-a backspace");
        cx.run_until_parked();
        assert_eq!(second(cx), second_row);

        // Settling the selected row moves it into the collapsed Settled section,
        // which sits below the Working rows.
        let bounds = cx
            .debug_bounds("settle-2")
            .expect("selected row shows Settle");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(cx.debug_bounds("settled-section").unwrap().origin.y > first_row.y);
        assert_eq!(top(cx), first_row);
    }

    #[gpui::test]
    fn shortcut_settlement_invalidates_cached_sidebar(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        for provider in [Provider::Claude, Provider::Codex] {
            let id = store
                .create(provider, &working_directory(), 1)
                .unwrap()
                .value;
            store.set_settled(id, true).unwrap();
        }
        store.select(1).unwrap();
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        view.update_in(cx, |this, window, cx| {
            this.focus.focus(window, cx);
            this.settled_open = true;
            this.refresh(cx);
        });
        cx.run_until_parked();
        let header = cx.debug_bounds("settled-section").unwrap().origin;
        let first_row = cx.debug_bounds("session-1").unwrap().origin;
        assert!(first_row.y > header.y);

        // No focus change or pointer event re-renders the sidebar here, only the refresh.
        // Un-settling session 1 moves it above the Settled header.
        cx.dispatch_action(ToggleSettle);
        cx.run_until_parked();
        let active_row = cx.debug_bounds("session-1").unwrap().origin.y;
        assert!(cx.debug_bounds("settled-section").unwrap().origin.y > active_row);
    }

    /// Runs the app's tasks, with real time passing, until `done` holds. Provider
    /// output arrives from a real child process, so simulated time alone cannot reach it.
    fn wait_until(
        cx: &mut gpui::VisualTestContext,
        view: &Entity<Keystone>,
        done: impl Fn(&Keystone, &App) -> bool,
    ) {
        cx.executor().allow_parking();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            cx.run_until_parked();
            if view.read_with(cx, |view, cx| done(view, cx)) {
                return;
            }
            if std::time::Instant::now() > deadline {
                view.read_with(cx, |view, cx| {
                    let state: Vec<_> = view
                        .state
                        .read(cx)
                        .conversations()
                        .iter()
                        .map(|(id, c)| {
                            (
                                id,
                                c.running,
                                c.rows.clone(),
                                c.error.clone(),
                                c.approvals.clone(),
                            )
                        })
                        .collect();
                    panic!(
                        "timed out; error {:?}; conversations {state:#?}",
                        view.error
                    );
                });
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn click(cx: &mut gpui::VisualTestContext, selector: &'static str) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} not rendered"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
    }

    /// Chooses an approval mode the way a user does: the shield button, then the mode,
    /// then Confirm when the mode asks for it.
    fn choose_mode(cx: &mut gpui::VisualTestContext, mode: ApprovalMode) {
        pick_mode(cx, mode);
        if mode == ApprovalMode::FullAccess {
            click(cx, "full-access-dialog-confirm");
        }
    }

    /// Picks a mode from the menu and stops there.
    fn pick_mode(cx: &mut gpui::VisualTestContext, mode: ApprovalMode) {
        click(cx, "approval-mode");
        click(cx, format!("approval-{}", mode.slug()).leak());
    }

    fn asking(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext) -> bool {
        view.read_with(cx, |view, _cx| view.confirm_full_access.is_some())
    }

    fn composer_focused(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext) -> bool {
        cx.update(|window, cx| view.read(cx).composer.focus_handle(cx).is_focused(window))
    }

    /// What the window's store and the saved file say about session 1, and how many
    /// times the store has written.
    fn saved_mode(
        view: &Entity<Keystone>,
        cx: &mut gpui::VisualTestContext,
    ) -> (ApprovalMode, ApprovalMode, u64) {
        view.read_with(cx, |view, cx| {
            let mode = |store: &Store| store.workspace().sessions()[0].approval_mode();
            let on_disk = Store::open(view.state.read(cx).store().path()).unwrap();
            (
                mode(&view.state.read(cx).store()),
                mode(&on_disk),
                view.state.read(cx).store().writes(),
            )
        })
    }

    fn send_text(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext, text: &str) {
        view.update_in(cx, |view, window, cx| {
            view.composer
                .update(cx, |input, cx| input.set_value(text.into(), cx));
            let draft = view.composer_draft(cx).unwrap();
            view.send_message(draft, window, cx);
        });
        cx.run_until_parked();
    }

    /// The text of the last completed reply that contains `text`, once the turn ended.
    fn reply_with(view: &Keystone, text: &str, cx: &App) -> Option<String> {
        let c = view.state.read(cx).conversations().get(&1)?;
        if c.running {
            return None;
        }
        c.rows.iter().rev().find_map(|row| match row {
            Row::Item(item)
                if item.kind == ItemKind::AgentMessage
                    && item.status == ItemStatus::Completed
                    && item.text.contains(text) =>
            {
                Some(item.text.clone())
            }
            _ => None,
        })
    }

    /// Waits for a completed reply containing `text` and returns the setting the scripted
    /// provider reports at its end: Codex's policy and sandbox, or Claude's mode.
    fn setting_of_reply(
        view: &Entity<Keystone>,
        cx: &mut gpui::VisualTestContext,
        text: &'static str,
    ) -> String {
        wait_until(cx, view, move |view, cx| {
            reply_with(view, text, cx).is_some()
        });
        let reply = view.read_with(cx, |view, cx| reply_with(view, text, cx).unwrap());
        let label = ["**Approvals:** `", "**Mode:** `"]
            .into_iter()
            .find(|label| reply.contains(label))
            .expect("a setting in the reply");
        let start = reply.find(label).unwrap() + label.len();
        reply[start..].split('`').next().unwrap().to_owned()
    }

    /// A wrapper that runs a scripted provider with extra environment, so tests sharing
    /// this process see no change.
    fn wrapper(dir: &Path, name: &str, env: &str) -> PathBuf {
        let script = dir.join(format!("{name}-wrapper"));
        std::fs::write(
            &script,
            format!("#!/bin/sh\n{env} exec '{}' \"$@\"\n", fake(name).display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        script
    }

    fn open_window<'a>(
        path: &Path,
        cx: &'a mut gpui::TestAppContext,
    ) -> (Entity<Keystone>, &'a mut gpui::VisualTestContext) {
        let store = Store::open(path).unwrap();
        cx.update(|cx| {
            keystone_ui::init(cx);
            bind_keys(cx);
        });
        cx.executor().allow_parking();
        cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        })
    }

    /// 2b in review: the record that the provider may hold an override is saved before
    /// the override can be sent. Choosing a mode saves it with the mode. A session saved
    /// with a mode but no record is not sent while the record cannot be saved.
    #[gpui::test]
    fn an_override_is_recorded_before_it_can_reach_the_provider(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let codex = wrapper(
            dir.path(),
            "keystone-fake-codex",
            "KEYSTONE_FAKE_CODEX_DELAY_MS=2",
        );
        std::fs::write(
            &path,
            r#"{"version":1,"next_id":3,"workspace":{"sessions":[
                {"id":1,"title":"Old","provider":"Codex","settled":false,"draft":"","approval_mode":"FullAccess"},
                {"id":2,"title":"New","provider":"Codex","settled":false,"draft":""}],"selected":2}}"#,
        )
        .unwrap();
        {
            let mut store = Store::open(&path).unwrap();
            for id in [1, 2] {
                store
                    .choose_legacy_directory(id, &working_directory())
                    .unwrap();
            }
        }
        let (view, cx) = open_window(&path, cx);
        view.update(cx, |view, cx| {
            view.state.update(cx, |state, _| {
                state.test_set_codex(Some(CodexConnection::new(
                    Some(codex),
                    "test",
                    state.test_wake().clone(),
                )));
            })
        });
        let saved = |id: u64| {
            Store::open(&path)
                .unwrap()
                .workspace()
                .sessions()
                .iter()
                .find(|s| s.id() == id)
                .unwrap()
                .clone()
        };
        choose_mode(cx, ApprovalMode::Auto);
        assert!(
            saved(2).approval_overridden(),
            "saved with the mode, before any turn"
        );

        view.update(cx, |view, cx| view.select(1, cx));
        cx.run_until_parked();
        let writer = lock_database(&path);
        send_text(&view, cx, "hello");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            let error = view.state.read(cx).conversations()[&1]
                .error
                .clone()
                .unwrap();
            assert!(error.starts_with("Not sent"), "{error}");
            assert_eq!(view.composer.read(cx).value(), "hello");
            assert!(
                !view.state.read(cx).test_codex().unwrap().is_open(1),
                "nothing reached Codex"
            );
        });
        drop(writer);
        cx.simulate_keystrokes("ctrl-enter");
        assert_eq!(
            setting_of_reply(&view, cx, "You wrote: hello"),
            "never / dangerFullAccess"
        );
        assert!(saved(1).approval_overridden());
    }

    /// 2c in review: a mode chosen from the menu reaches work already handed to Codex.
    /// A steer sent during a Full access turn reaches Codex as the turn ends; Codex
    /// rejects it and the driver sends it as the next turn, which must use Read only.
    #[gpui::test]
    fn a_mode_chosen_from_the_menu_reaches_a_steer_that_becomes_the_next_turn(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let codex = wrapper(
            dir.path(),
            "keystone-fake-codex",
            "KEYSTONE_FAKE_CODEX_DELAY_MS=20",
        );
        let (view, cx) = open_window(&path, cx);
        view.update(cx, |view, cx| {
            view.state.update(cx, |state, _| {
                state.test_set_codex(Some(CodexConnection::new(
                    Some(codex),
                    "test",
                    state.test_wake().clone(),
                )));
            })
        });
        click(cx, "welcome-codex");
        choose_mode(cx, ApprovalMode::FullAccess);
        send_text(&view, cx, "end on steer");
        wait_until(cx, &view, |view, cx| {
            view.state.read(cx).conversations()[&1]
                .rows
                .iter()
                .any(|row| {
                    matches!(row, Row::Item(item)
                    if item.kind == ItemKind::AgentMessage && !item.text.is_empty())
                })
        });
        send_text(&view, cx, "late");
        // Codex holds the steer for 300 ms; choose while it waits there.
        std::thread::sleep(std::time::Duration::from_millis(100));
        choose_mode(cx, ApprovalMode::ReadOnly);
        assert_eq!(
            setting_of_reply(&view, cx, "You wrote: late"),
            "on-request / readOnly"
        );
    }

    /// 2a in review: Codex exits after the user chose Read only on a thread that ran with
    /// Full access. The next message starts a new app-server, which must still get Read
    /// only; the resumed thread would otherwise keep Full access. After Keystone
    /// restarts, Provider settings restore the user's own settings.
    #[gpui::test]
    fn a_mode_survives_a_codex_restart_and_provider_settings_restore_after_one(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let codex = wrapper(
            dir.path(),
            "keystone-fake-codex",
            &format!(
                "KEYSTONE_FAKE_CODEX_DELAY_MS=2 KEYSTONE_FAKE_CODEX_STATE='{}'",
                dir.path().join("codex-state.json").display()
            ),
        );
        let (view, cx) = open_window(&path, cx);
        view.update(cx, |view, cx| {
            view.state.update(cx, |state, _| {
                state.test_set_codex(Some(CodexConnection::new(
                    Some(codex.clone()),
                    "test",
                    state.test_wake().clone(),
                )));
            })
        });
        click(cx, "welcome-codex");
        choose_mode(cx, ApprovalMode::FullAccess);
        send_text(&view, cx, "first");
        assert_eq!(
            setting_of_reply(&view, cx, "You wrote: first"),
            "never / dangerFullAccess"
        );

        choose_mode(cx, ApprovalMode::ReadOnly);
        let pid = view.read_with(cx, |view, cx| {
            view.state.read(cx).test_codex().unwrap().pid().unwrap()
        });
        std::process::Command::new("kill")
            .arg(pid.to_string())
            .status()
            .unwrap();
        wait_until(cx, &view, |view, cx| {
            view.state.read(cx).test_codex().unwrap().pid().is_none()
        });
        send_text(&view, cx, "second");
        assert_eq!(
            setting_of_reply(&view, cx, "You wrote: second"),
            "on-request / readOnly"
        );

        // Keystone restarts. The thread still holds Read only, so Provider settings must
        // send the user's own.
        view.update(cx, |view, cx| view.flush_logs(cx));
        let store = Store::open(&path).unwrap();
        cx.executor().allow_parking();
        let (restored, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        restored.update(cx, |view, cx| {
            view.state.update(cx, |state, _| {
                state.test_set_codex(Some(CodexConnection::new(
                    Some(codex),
                    "test",
                    state.test_wake().clone(),
                )));
            })
        });
        wait_until(cx, &restored, |view, cx| {
            view.state
                .read(cx)
                .conversations()
                .get(&1)
                .is_some_and(|c| !c.is_loading())
        });
        choose_mode(cx, ApprovalMode::Provider);
        send_text(&restored, cx, "third");
        assert_eq!(
            setting_of_reply(&restored, cx, "You wrote: third"),
            "on-request / workspaceWrite"
        );
    }

    /// Full access asks first. Cancel, Escape, Enter (Cancel has focus), and a press
    /// outside the dialog each leave the mode as it was: nothing is saved, the
    /// connection keeps its state, and the next turn runs under the old mode.
    #[gpui::test]
    fn full_access_changes_nothing_until_confirmed(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let codex = wrapper(
            dir.path(),
            "keystone-fake-codex",
            "KEYSTONE_FAKE_CODEX_DELAY_MS=2",
        );
        let (view, cx) = open_window(&path, cx);
        view.update(cx, |view, cx| {
            view.state.update(cx, |state, _| {
                state.test_set_codex(Some(CodexConnection::new(
                    Some(codex),
                    "test",
                    state.test_wake().clone(),
                )));
            })
        });
        click(cx, "welcome-codex");
        choose_mode(cx, ApprovalMode::Auto);
        send_text(&view, cx, "first");
        assert_eq!(
            setting_of_reply(&view, cx, "You wrote: first"),
            "on-request / workspaceWrite"
        );
        let connection = |view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext| {
            view.read_with(cx, |view, cx| {
                view.state.read(cx).test_codex().unwrap().approval(1)
            })
        };
        let auto = Some(ApprovalState {
            mode: ApprovalMode::Auto,
            overridden: true,
        });
        assert_eq!(connection(&view, cx), auto);
        let before = saved_mode(&view, cx);
        let file = Store::open(&path).unwrap().saved_snapshot().unwrap();
        for how in ["cancel", "escape", "enter", "outside"] {
            pick_mode(cx, ApprovalMode::FullAccess);
            assert!(asking(&view, cx), "{how}: the dialog shows");
            assert!(cx.debug_bounds("full-access-dialog").is_some(), "{how}");
            assert!(!composer_focused(&view, cx), "{how}: the dialog has focus");
            match how {
                "cancel" => click(cx, "full-access-dialog-cancel"),
                "escape" => cx.simulate_keystrokes("escape"),
                "enter" => {
                    cx.simulate_keystrokes("enter");
                    cx.simulate_event(gpui::KeyUpEvent {
                        keystroke: gpui::Keystroke::parse("enter").unwrap(),
                    });
                }
                _ => {
                    cx.simulate_click(gpui::point(px(4.), px(4.)), gpui::Modifiers::none());
                    cx.run_until_parked();
                }
            }
            assert!(!asking(&view, cx), "{how}: the dialog closed");
            assert!(composer_focused(&view, cx), "{how}: focus is back");
            assert_eq!(saved_mode(&view, cx), before, "{how}: nothing saved");
            assert_eq!(
                Store::open(&path).unwrap().saved_snapshot().unwrap(),
                file,
                "{how}"
            );
            assert_eq!(connection(&view, cx), auto, "{how}: nothing sent");
        }
        send_text(&view, cx, "second");
        assert_eq!(
            setting_of_reply(&view, cx, "You wrote: second"),
            "on-request / workspaceWrite"
        );
    }

    /// Work already handed to Codex keeps the old mode while the dialog is open: a
    /// steer Codex rejects becomes the next turn, under Read only. While the dialog is
    /// open the shortcut sends nothing. Confirm saves Full access once, gives it to the
    /// connection, and the next turn runs in it.
    #[gpui::test]
    fn pending_work_never_runs_in_full_access_before_confirm(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let codex = wrapper(
            dir.path(),
            "keystone-fake-codex",
            "KEYSTONE_FAKE_CODEX_DELAY_MS=20",
        );
        let (view, cx) = open_window(&path, cx);
        view.update(cx, |view, cx| {
            view.state.update(cx, |state, _| {
                state.test_set_codex(Some(CodexConnection::new(
                    Some(codex),
                    "test",
                    state.test_wake().clone(),
                )));
            })
        });
        click(cx, "welcome-codex");
        choose_mode(cx, ApprovalMode::ReadOnly);
        send_text(&view, cx, "end on steer");
        wait_until(cx, &view, |view, cx| {
            view.state.read(cx).conversations()[&1]
                .rows
                .iter()
                .any(|row| {
                    matches!(row, Row::Item(item)
                    if item.kind == ItemKind::AgentMessage && !item.text.is_empty())
                })
        });
        send_text(&view, cx, "late");
        // Codex holds the steer for 300 ms; pick Full access while it waits there.
        pick_mode(cx, ApprovalMode::FullAccess);
        assert!(asking(&view, cx));
        assert_eq!(
            setting_of_reply(&view, cx, "You wrote: late"),
            "on-request / readOnly"
        );
        view.update(cx, |view, cx| {
            view.composer
                .update(cx, |input, cx| input.set_value("blocked".into(), cx))
        });
        cx.simulate_keystrokes("ctrl-enter");
        view.read_with(cx, |view, cx| {
            assert_eq!(view.composer.read(cx).value(), "blocked", "not sent");
        });
        let (_, _, writes) = saved_mode(&view, cx);
        click(cx, "full-access-dialog-confirm");
        assert!(!asking(&view, cx));
        assert!(composer_focused(&view, cx));
        let full = ApprovalMode::FullAccess;
        assert_eq!(
            saved_mode(&view, cx),
            (full, full, writes + 1),
            "saved once"
        );
        view.read_with(cx, |view, cx| {
            assert!(view.state.read(cx).store().workspace().sessions()[0].approval_overridden());
            assert_eq!(
                view.state.read(cx).test_codex().unwrap().approval(1),
                Some(ApprovalState {
                    mode: full,
                    overridden: true
                })
            );
        });
        cx.simulate_keystrokes("ctrl-enter");
        assert_eq!(
            setting_of_reply(&view, cx, "You wrote: blocked"),
            "never / dangerFullAccess"
        );
    }

    /// Only entering Full access from the menu asks. A saved Full access session opens
    /// without the dialog, choosing it again changes nothing, and leaving it applies at
    /// once.
    #[gpui::test]
    fn only_entering_full_access_from_the_menu_asks(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        std::fs::write(
            &path,
            r#"{"version":1,"next_id":2,"workspace":{"sessions":[
                {"id":1,"title":"Full","provider":"Codex","settled":false,"draft":"",
                 "approval_mode":"FullAccess","approval_overridden":true}],"selected":1}}"#,
        )
        .unwrap();
        let (view, cx) = open_window(&path, cx);
        cx.run_until_parked();
        assert!(!asking(&view, cx), "restoring a saved mode asks nothing");
        assert!(cx.debug_bounds("full-access-dialog").is_none());
        let full = ApprovalMode::FullAccess;
        let launched = saved_mode(&view, cx).2;
        assert_eq!(saved_mode(&view, cx), (full, full, launched));

        pick_mode(cx, full);
        assert!(!asking(&view, cx), "already in Full access");
        assert_eq!(
            saved_mode(&view, cx),
            (full, full, launched),
            "nothing saved"
        );

        let read_only = ApprovalMode::ReadOnly;
        pick_mode(cx, read_only);
        assert!(!asking(&view, cx), "leaving Full access asks nothing");
        assert_eq!(saved_mode(&view, cx), (read_only, read_only, launched + 1));
    }

    /// For Claude, Cancel keeps the running process and its mode; only Confirm replaces
    /// it with one in Full access.
    #[gpui::test]
    fn claude_keeps_its_process_until_full_access_is_confirmed(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let claude = wrapper(
            dir.path(),
            "keystone-fake-claude",
            "KEYSTONE_FAKE_CLAUDE_DELAY_MS=5",
        );
        let (view, cx) = open_window(&path, cx);
        view.update(cx, |view, cx| {
            view.state.update(cx, |state, _| {
                state.test_set_claude(Some(ClaudeConnection::new(
                    Some(claude),
                    state.test_wake().clone(),
                )));
            })
        });
        let pid = |view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext| {
            view.read_with(cx, |view, cx| {
                view.state.read(cx).test_claude().unwrap().pid(1)
            })
        };
        click(cx, "welcome-claude");
        choose_mode(cx, ApprovalMode::Auto);
        send_text(&view, cx, "hello");
        assert_eq!(
            setting_of_reply(&view, cx, "You wrote: hello"),
            "acceptEdits"
        );
        let first = pid(&view, cx);

        pick_mode(cx, ApprovalMode::FullAccess);
        click(cx, "full-access-dialog-cancel");
        send_text(&view, cx, "again");
        assert_eq!(
            setting_of_reply(&view, cx, "You wrote: again"),
            "acceptEdits"
        );
        assert_eq!(pid(&view, cx), first, "Cancel kept the process");

        choose_mode(cx, ApprovalMode::FullAccess);
        send_text(&view, cx, "full");
        assert_eq!(
            setting_of_reply(&view, cx, "You wrote: full"),
            "bypassPermissions"
        );
        assert_ne!(pid(&view, cx), first, "Confirm replaced it");
    }

    /// Claude sessions apply each mode from the menu. Full access and a return to
    /// Provider settings need a new `claude` process, which resumes the conversation. A
    /// change during a turn applies from the next turn.
    #[gpui::test]
    fn claude_sessions_apply_approval_modes_from_the_menu(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let claude = wrapper(
            dir.path(),
            "keystone-fake-claude",
            "KEYSTONE_FAKE_CLAUDE_DELAY_MS=5",
        );
        let (view, cx) = open_window(&path, cx);
        view.update(cx, |view, cx| {
            view.state.update(cx, |state, _| {
                state.test_set_claude(Some(ClaudeConnection::new(
                    Some(claude),
                    state.test_wake().clone(),
                )));
            })
        });
        let pid = |view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext| {
            view.read_with(cx, |view, cx| {
                view.state.read(cx).test_claude().unwrap().pid(1)
            })
        };
        click(cx, "welcome-claude");
        choose_mode(cx, ApprovalMode::Auto);
        send_text(&view, cx, "hello");
        assert_eq!(
            setting_of_reply(&view, cx, "You wrote: hello"),
            "acceptEdits"
        );
        let first = pid(&view, cx);

        // A change while a turn runs waits for the next turn.
        send_text(&view, cx, "long");
        wait_until(cx, &view, |view, cx| {
            view.state.read(cx).conversations()[&1].running
        });
        choose_mode(cx, ApprovalMode::ReadOnly);
        assert_eq!(
            setting_of_reply(&view, cx, "You wrote: long"),
            "acceptEdits"
        );
        send_text(&view, cx, "next");
        assert_eq!(setting_of_reply(&view, cx, "You wrote: next"), "plan");
        assert_eq!(pid(&view, cx), first, "changed in place");

        // Back to the user's own settings, in a process launched without a mode.
        choose_mode(cx, ApprovalMode::Provider);
        send_text(&view, cx, "back");
        assert_eq!(setting_of_reply(&view, cx, "You wrote: back"), "default");
        let own = pid(&view, cx);
        assert_ne!(own, first);

        // Full access runs the command without asking, in a new process.
        choose_mode(cx, ApprovalMode::FullAccess);
        send_text(&view, cx, "please run it");
        assert_eq!(
            setting_of_reply(&view, cx, "You wrote: please run it"),
            "bypassPermissions"
        );
        view.read_with(cx, |view, cx| {
            assert!(view.state.read(cx).conversations()[&1].rows.iter().any(|row| matches!(row,
                Row::Item(item) if matches!(item.kind, ItemKind::Command { exit_code: Some(0), .. }))));
            assert!(!view.state.read(cx).conversations()[&1].rows.iter().any(|row| matches!(row, Row::Decision { .. })));
        });
        let full = pid(&view, cx);
        assert_ne!(full, own);

        // Leaving Full access needs a new process too.
        choose_mode(cx, ApprovalMode::Ask);
        send_text(&view, cx, "last");
        assert_eq!(setting_of_reply(&view, cx, "You wrote: last"), "default");
        assert_ne!(pid(&view, cx), full);
        let thread = Store::open(&path).unwrap().workspace().sessions()[0]
            .provider_thread()
            .map(str::to_owned);
        assert!(thread.is_some_and(|t| t.starts_with("fake-claude-session-")));
    }

    /// A scripted provider built by `keystone-agent`. Test binaries live in
    /// target/<profile>/deps; the scripts are one level up. `scripts/check.sh ui` builds
    /// them first.
    fn fake(name: &str) -> PathBuf {
        let exe = std::env::current_exe().unwrap();
        let path = exe.parent().unwrap().parent().unwrap().join(name);
        assert!(
            path.exists(),
            "build it with `cargo build -p keystone-agent --bins`"
        );
        path
    }

    /// Points Keystone at the scripted Codex. The variables are process-wide, so every
    /// test that sets them holds this lock until it ends, and one test's switches
    /// never reach a process another test starts.
    fn codex_env() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // SAFETY: set under the lock, before any thread that reads them starts.
        unsafe {
            std::env::set_var("KEYSTONE_CODEX_PATH", fake("keystone-fake-codex"));
            std::env::set_var("KEYSTONE_FAKE_CODEX_DELAY_MS", "2");
        }
        guard
    }

    /// Removes a variable when the test ends, however it ends.
    struct EnvVar(&'static str);

    impl Drop for EnvVar {
        fn drop(&mut self) {
            // SAFETY: the test that set it holds the lock that `codex_env` takes.
            unsafe { std::env::remove_var(self.0) }
        }
    }

    /// Waits until `selector` renders, letting provider events arrive meanwhile.
    fn wait_for(cx: &mut gpui::VisualTestContext, selector: &'static str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            cx.run_until_parked();
            if cx.debug_bounds(selector).is_some() {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{selector} never rendered"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn saved_session(path: &Path) -> keystone_core::Session {
        Store::open(path).unwrap().workspace().sessions()[0].clone()
    }

    #[gpui::test]
    fn a_draft_chooses_a_model_the_turn_runs_on_and_the_first_send_fixes_it(
        cx: &mut gpui::TestAppContext,
    ) {
        let _env = codex_env();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let store = Store::open(&path).unwrap();
        cx.update(|cx| {
            keystone_ui::init(cx);
            bind_keys(cx);
        });
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        click(cx, "welcome-codex");
        click(cx, "model-selector");
        assert!(cx.debug_bounds("provider-claude").is_some());
        assert!(cx.debug_bounds("model-default").is_some());
        wait_for(cx, "model-fake-fast");
        assert!(cx.debug_bounds("model-fake-hidden").is_none());
        click(cx, "model-fake-fast");
        assert_eq!(saved_session(&path).model(), Some("fake-fast"));
        assert!(view.read_with(cx, |view, _cx| view.model_menu.is_none()));
        assert!(composer_focused(&view, cx));

        send_text(&view, cx, "hello");
        wait_until(cx, &view, |view, cx| {
            reply_with(view, "**Model:** `fake-fast`", cx).is_some()
        });
        let session = saved_session(&path);
        assert_eq!(session.model_locked(), Some(true));
        assert!(session.started());
        view.read_with(cx, |view, cx| {
            assert_eq!(
                view.state.read(cx).conversations()[&1]
                    .reported_model
                    .as_deref(),
                Some("fake-fast")
            );
        });
        // Fixed now: the button opens nothing.
        click(cx, "model-selector");
        assert!(cx.debug_bounds("provider-claude").is_none());
        assert!(view.read_with(cx, |view, _cx| view.model_menu.is_none()));
    }

    #[gpui::test]
    fn switching_the_provider_in_the_menu_retitles_the_draft_and_lists_its_models(
        cx: &mut gpui::TestAppContext,
    ) {
        let _env = codex_env();
        // SAFETY: the same values every Claude test sets, under the Codex lock.
        unsafe {
            std::env::set_var("KEYSTONE_CLAUDE_PATH", fake("keystone-fake-claude"));
            std::env::set_var("KEYSTONE_FAKE_CLAUDE_DELAY_MS", "2");
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let store = Store::open(&path).unwrap();
        cx.update(|cx| {
            keystone_ui::init(cx);
            bind_keys(cx);
        });
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        click(cx, "welcome-codex");
        click(cx, "model-selector");
        wait_for(cx, "model-fake-fast");
        click(cx, "model-fake-fast");
        click(cx, "model-selector");
        click(cx, "provider-claude");
        let session = saved_session(&path);
        assert_eq!(session.provider(), Provider::Claude);
        assert_eq!(session.title(), "New Claude session");
        assert_eq!(session.model(), None);
        assert!(view.read_with(cx, |view, _cx| view.model_menu.is_some()));
        wait_for(cx, "model-fake-opus");
        let default = cx.debug_bounds("model-default").unwrap();
        let opus = cx.debug_bounds("model-fake-opus").unwrap();
        let column = cx.debug_bounds("model-column").unwrap();
        assert!(
            default.size.height == opus.size.height && opus.right() <= column.right(),
            "default {default:?}, a model row {opus:?}, column {column:?}"
        );
        click(cx, "model-fake-opus");
        assert_eq!(saved_session(&path).model(), Some("fake-opus"));

        send_text(&view, cx, "hello");
        wait_until(cx, &view, |view, cx| {
            reply_with(view, "**Model:** `fake-opus`", cx).is_some()
        });
    }

    #[gpui::test]
    fn a_catalog_codex_cannot_list_shows_the_failure_and_keeps_the_default(
        cx: &mut gpui::TestAppContext,
    ) {
        let _env = codex_env();
        // SAFETY: under the Codex lock, removed when the test ends.
        unsafe { std::env::set_var("KEYSTONE_FAKE_CODEX_NO_CATALOG", "1") };
        let _switch = EnvVar("KEYSTONE_FAKE_CODEX_NO_CATALOG");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let store = Store::open(&path).unwrap();
        cx.update(|cx| {
            keystone_ui::init(cx);
            bind_keys(cx);
        });
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        click(cx, "welcome-codex");
        click(cx, "model-selector");
        wait_for(cx, "model-failed");
        assert!(cx.debug_bounds("model-retry").is_some());
        assert!(cx.debug_bounds("model-default").is_some());
        click(cx, "model-retry");
        wait_for(cx, "model-failed");
        click(cx, "model-default");
        assert!(view.read_with(cx, |view, _cx| view.model_menu.is_none()));
        assert!(composer_focused(&view, cx));
        assert_eq!(saved_session(&path).model(), None);
    }

    /// The menu belongs to the draft it opened on. The first send closes it, and a new
    /// session starts with its own menu closed, so no row of the old draft's menu is
    /// shown over the new one.
    #[gpui::test]
    fn the_model_menu_closes_with_the_first_send_and_stays_closed_for_a_new_session(
        cx: &mut gpui::TestAppContext,
    ) {
        let _env = codex_env();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let (view, cx) = open_window(&path, cx);
        click(cx, "welcome-codex");
        click(cx, "model-selector");
        wait_for(cx, "model-fake-fast");
        assert_eq!(view.read_with(cx, |view, _cx| view.model_menu), Some(1));
        view.update_in(cx, |view, window, cx| {
            view.composer
                .update(cx, |input, cx| input.set_value("hello".into(), cx));
            view.composer.focus_handle(cx).focus(window, cx);
        });
        cx.simulate_keystrokes("ctrl-enter");
        cx.run_until_parked();
        assert!(saved_session(&path).started(), "the message went");
        assert_eq!(view.read_with(cx, |view, _cx| view.model_menu), None);
        assert!(cx.debug_bounds("provider-claude").is_none());

        cx.simulate_keystrokes("ctrl-shift-n");
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, cx| view
                .state
                .read(cx)
                .store()
                .workspace()
                .selected()),
            Some(2)
        );
        assert!(cx.debug_bounds("provider-claude").is_none());
        click(cx, "model-selector");
        assert_eq!(view.read_with(cx, |view, _cx| view.model_menu), Some(2));
        assert!(cx.debug_bounds("provider-claude").is_some());
    }

    #[gpui::test]
    fn a_refused_first_send_closes_the_model_menu_with_the_lock(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let claude = wrapper(
            dir.path(),
            "keystone-fake-claude",
            "KEYSTONE_FAKE_CLAUDE_DELAY_MS=5",
        );
        std::fs::write(dir.path().join(".fake-claude-ignore-initialize"), "all").unwrap();
        let (view, cx) = open_window(&path, cx);
        view.update(cx, |view, cx| {
            view.state.update(cx, |state, _| {
                state.test_set_claude(Some(ClaudeConnection::new(
                    Some(claude),
                    state.test_wake().clone(),
                )));
            });
            view.presentation.project =
                keystone_core::ProjectId::new(dir.path()).map_err(|e| e.to_string());
        });
        click(cx, "welcome-claude");
        click(cx, "model-selector");
        assert_eq!(view.read_with(cx, |view, _cx| view.model_menu), Some(1));
        view.update_in(cx, |view, window, cx| {
            let Some(presentation::ConversationFolder::Recorded(cwd)) =
                view.conversation_folder(1, cx)
            else {
                panic!("a new session records its folder");
            };
            view.state
                .read(cx)
                .test_claude()
                .unwrap()
                .send(
                    1,
                    cwd.as_str(),
                    None,
                    None,
                    ApprovalState::default(),
                    "first".into(),
                )
                .unwrap();
            view.composer
                .update(cx, |input, cx| input.set_value("hello".into(), cx));
            let draft = view.composer_draft(cx).unwrap();
            view.send_message(draft, window, cx);
            assert!(
                view.state.read(cx).conversations()[&1]
                    .error
                    .as_deref()
                    .is_some_and(|e| e.contains("cannot steer")),
                "{:?}",
                view.state.read(cx).conversations()[&1].error
            );
            assert_eq!(view.model_menu, None);
        });
        cx.run_until_parked();
        assert!(saved_session(&path).started(), "the lock was saved");
        assert!(cx.debug_bounds("provider-claude").is_none());
    }

    /// The model button keeps its full label whatever the hint beside it needs: the
    /// hint gives way, inside the toolbar, and nothing overlaps the project button. The
    /// composer is 640 px wide at any window size, so this is measured at 1280 × 900.
    #[gpui::test]
    fn the_model_button_keeps_its_label_and_the_hint_gives_way(cx: &mut gpui::TestAppContext) {
        let _env = codex_env();
        // SAFETY: the same values every Claude test sets, under the Codex lock.
        unsafe {
            std::env::set_var("KEYSTONE_CLAUDE_PATH", fake("keystone-fake-claude"));
            std::env::set_var("KEYSTONE_FAKE_CLAUDE_DELAY_MS", "2");
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let (_view, cx) = open_window(&path, cx);
        cx.simulate_resize(size(px(1280.), px(900.)));
        click(cx, "welcome-codex");
        let laid_out = |cx: &mut gpui::VisualTestContext| {
            let button = cx.debug_bounds("model-selector").unwrap();
            let hint = cx.debug_bounds("composer-hint").unwrap();
            let project = cx.debug_bounds("project").unwrap();
            assert!(
                button.right() <= hint.origin.x && hint.right() <= project.origin.x,
                "button {button:?}, hint {hint:?}, project {project:?}"
            );
        };
        laid_out(cx);

        click(cx, "model-selector");
        click(cx, "provider-claude");
        wait_for(cx, "model-fake-opus");
        click(cx, "model-fake-opus");
        laid_out(cx);
    }

    #[gpui::test]
    fn codex_session_streams_asks_approval_and_restores_from_its_log(
        cx: &mut gpui::TestAppContext,
    ) {
        let _env = codex_env();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let store = Store::open(&path).unwrap();
        cx.update(|cx| {
            keystone_ui::init(cx);
            bind_keys(cx);
        });
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        let bounds = cx.debug_bounds("welcome-codex").unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.simulate_input("hello");
        let bounds = cx.debug_bounds("send").unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());

        let finished = |view: &Keystone, cx: &App| {
            view.state.read(cx).conversations().get(&1).is_some_and(|c| {
                !c.running
                    && c.rows.iter().any(|row| matches!(row, Row::Item(item)
                        if item.kind == ItemKind::AgentMessage && item.status == ItemStatus::Completed))
            })
        };
        wait_until(cx, &view, finished);
        view.read_with(cx, |view, cx| {
            assert_eq!(view.composer.read(cx).value(), "");
            let rows = &view.state.read(cx).conversations()[&1].rows;
            assert!(matches!(&rows[0], Row::Item(item) if item.kind == ItemKind::UserMessage && item.text == "hello"));
            // No mode chosen: Keystone sent no override, so Codex used its own settings.
            assert!(rows.iter().any(|row| matches!(row, Row::Item(item)
                if item.text.contains("You wrote: hello") && item.text.contains("`on-request / workspaceWrite`"))));
        });
        // The reply renders as Markdown, with a copy action once it is complete.
        let reply = view.read_with(cx, |view, cx| {
            view.state.read(cx).conversations()[&1]
                .rows
                .iter()
                .position(
                    |row| matches!(row, Row::Item(item) if item.kind == ItemKind::AgentMessage),
                )
                .unwrap()
        });
        view.read_with(cx, |view, cx| {
            let Row::Item(item) = &view.state.read(cx).conversations()[&1].rows[reply] else {
                panic!("a reply row");
            };
            assert!(
                !view
                    .replies
                    .borrow_mut()
                    .document((1, reply), item)
                    .is_empty()
            );
        });
        click(cx, format!("copy-reply-{reply}").leak());
        let copied = cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()));
        assert!(copied.unwrap().contains("**Approvals:**"));
        hover(cx, "transcript-row-0");
        click(cx, "copy-message-0");
        let copied = cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()));
        assert_eq!(copied.as_deref(), Some("hello"));
        view.update_in(cx, |view, window, cx| {
            view.composer.focus_handle(cx).focus(window, cx)
        });
        assert_eq!(
            Store::open(&path).unwrap().workspace().sessions()[0].provider_thread(),
            Some("fake-thread-1")
        );

        // The shortcut sends too. A command waits for approval.
        cx.simulate_input("please run it");
        cx.simulate_keystrokes("ctrl-enter");
        wait_until(cx, &view, |view, cx| {
            !view.state.read(cx).conversations()[&1].approvals.is_empty()
        });
        let bounds = cx.debug_bounds("approve").expect("approval card rendered");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        // Answering hands focus back to the composer.
        cx.simulate_input("next");
        view.read_with(cx, |view, cx| {
            assert_eq!(view.composer.read(cx).value(), "next")
        });
        wait_until(cx, &view, |view, cx| {
            let c = &view.state.read(cx).conversations()[&1];
            !c.running
                && c.rows.iter().any(|row| {
                    matches!(row, Row::Item(item)
                if matches!(item.kind, ItemKind::Command { exit_code: Some(0), .. }))
                })
        });

        // A fresh window reads the conversation back from the session's log.
        view.update(cx, |view, cx| view.flush_logs(cx));
        let store = Store::open(&path).unwrap();
        cx.executor().allow_parking();
        let (restored, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        wait_until(cx, &restored, |view, cx| {
            view.state
                .read(cx)
                .conversations()
                .get(&1)
                .is_some_and(|c| !c.is_loading())
        });
        restored.read_with(cx, |view, cx| {
            let rows = &view.state.read(cx).conversations()[&1].rows;
            assert!(rows.contains(&Row::Decision { action: "echo scripted".into(), allowed: true }));
            assert!(rows.iter().any(|row| matches!(row, Row::Item(item) if item.text.contains("You wrote: please run it"))));
        });
    }

    #[gpui::test]
    fn claude_session_streams_asks_approval_stops_and_restores_from_its_log(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        // SAFETY: only this test reads these variables, and it sets them before any
        // thread that could read them starts.
        unsafe {
            std::env::set_var("KEYSTONE_CLAUDE_PATH", fake("keystone-fake-claude"));
            std::env::set_var("KEYSTONE_FAKE_CLAUDE_DELAY_MS", "2");
        }
        let store = Store::open(&path).unwrap();
        cx.update(|cx| {
            keystone_ui::init(cx);
            bind_keys(cx);
        });
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        let bounds = cx.debug_bounds("welcome-claude").unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.simulate_input("hello");
        let bounds = cx.debug_bounds("send").expect("Claude sessions can send");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());

        let replied = |text: &'static str| {
            move |view: &Keystone, cx: &App| {
                view.state
                    .read(cx)
                    .conversations()
                    .get(&1)
                    .is_some_and(|c| {
                        !c.running
                            && c.rows.iter().any(|row| {
                                matches!(row, Row::Item(item)
                            if item.kind == ItemKind::AgentMessage && item.text.contains(text))
                            })
                    })
            }
        };
        wait_until(cx, &view, replied("You wrote: hello"));
        view.read_with(cx, |view, cx| {
            assert_eq!(view.composer.read(cx).value(), "");
            let rows = &view.state.read(cx).conversations()[&1].rows;
            // The local copy was replaced by Claude's echo, not shown twice.
            let users = rows
                .iter()
                .filter(|row| {
                    matches!(row, Row::Item(item)
                if item.kind == ItemKind::UserMessage)
                })
                .count();
            assert_eq!(users, 1);
        });
        let thread = Store::open(&path).unwrap().workspace().sessions()[0]
            .provider_thread()
            .map(str::to_owned);
        assert!(thread.is_some_and(|t| t.starts_with("fake-claude-session-")));

        // A command waits for approval.
        cx.simulate_input("please run it");
        cx.simulate_keystrokes("ctrl-enter");
        wait_until(cx, &view, |view, cx| {
            !view.state.read(cx).conversations()[&1].approvals.is_empty()
        });
        let bounds = cx.debug_bounds("approve").expect("approval card rendered");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        wait_until(cx, &view, |view, cx| {
            let c = &view.state.read(cx).conversations()[&1];
            !c.running
                && c.rows.iter().any(|row| {
                    matches!(row, Row::Item(item)
                if matches!(item.kind, ItemKind::Command { exit_code: Some(0), .. }))
                })
        });

        // Stop ends a long reply and keeps what streamed.
        cx.simulate_input("a long one");
        cx.simulate_keystrokes("ctrl-enter");
        wait_until(cx, &view, |view, cx| {
            view.state.read(cx).conversations()[&1]
                .rows
                .iter()
                .any(|row| {
                    matches!(row, Row::Item(item)
                if item.status == ItemStatus::InProgress && item.text.contains("word1"))
                })
        });
        let bounds = cx.debug_bounds("stop").expect("stop shown while running");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        wait_until(cx, &view, |view, cx| {
            let c = &view.state.read(cx).conversations()[&1];
            !c.running
                && matches!(
                    c.rows.last(),
                    Some(Row::TurnEnded {
                        status: TurnStatus::Interrupted,
                        ..
                    })
                )
        });

        // A fresh window reads the conversation back from the session's log.
        view.update(cx, |view, cx| view.flush_logs(cx));
        let store = Store::open(&path).unwrap();
        cx.executor().allow_parking();
        let (restored, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        wait_until(cx, &restored, |view, cx| {
            view.state
                .read(cx)
                .conversations()
                .get(&1)
                .is_some_and(|c| !c.is_loading())
        });
        restored.read_with(cx, |view, cx| {
            let rows = &view.state.read(cx).conversations()[&1].rows;
            assert!(rows.contains(&Row::Decision {
                action: "echo scripted".into(),
                allowed: true
            }));
            assert!(rows.iter().any(
                |row| matches!(row, Row::Item(item) if item.text.contains("You wrote: hello"))
            ));
            // Claude completes the cut-off block itself, so the partial reply is kept.
            assert!(rows.iter().any(|row| matches!(row, Row::Item(item)
                if item.kind == ItemKind::AgentMessage && item.text.contains("word1"))));
        });
    }

    #[gpui::test]
    fn a_provider_that_cannot_start_shows_an_error_that_stays(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("workspace.json")).unwrap();
        cx.update(|cx| {
            keystone_ui::init(cx);
            bind_keys(cx);
        });
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        view.update(cx, |view, cx| {
            view.state.update(cx, |state, _| {
                state.test_set_claude(Some(ClaudeConnection::new(
                    Some(PathBuf::from("/nonexistent/claude")),
                    state.test_wake().clone(),
                )));
            })
        });
        let bounds = cx.debug_bounds("welcome-claude").unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.simulate_input("hello");
        cx.simulate_keystrokes("ctrl-enter");
        // The failure arrives from the connection's worker, after the send returned.
        wait_until(cx, &view, |view, cx| {
            view.state
                .read(cx)
                .conversations()
                .get(&1)
                .is_some_and(|c| c.error.is_some())
        });
        // The draft save that follows typing must not hide the failure.
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(300));
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            let conversation = &view.state.read(cx).conversations()[&1];
            let error = conversation.error.clone().unwrap_or_default();
            assert!(
                error.starts_with("Could not start Claude Code at /nonexistent/claude."),
                "{error}"
            );
            assert!(!conversation.running);
            // The message stays in the transcript, and the error is shown once.
            assert!(
                conversation
                    .rows
                    .iter()
                    .any(|row| matches!(row, Row::Item(item)
                if item.kind == ItemKind::UserMessage && item.text == "hello"))
            );
            assert_eq!(view.error, None);
        });
    }

    /// A Claude without lifecycle frames cannot be steered. The refusal shows its own
    /// reason, not a start failure, and the text stays in the composer.
    #[gpui::test]
    fn a_refused_steer_shows_why_and_keeps_the_text(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("workspace.json")).unwrap();
        // The setting goes in a wrapper, so tests sharing this process see no change.
        let script = dir.path().join("old-claude");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nKEYSTONE_FAKE_CLAUDE_NO_LIFECYCLE=1 KEYSTONE_FAKE_CLAUDE_DELAY_MS=5 exec '{}' \"$@\"\n",
                fake("keystone-fake-claude").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        cx.update(|cx| {
            keystone_ui::init(cx);
            bind_keys(cx);
        });
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        view.update(cx, |view, cx| {
            view.state.update(cx, |state, _| {
                state.test_set_claude(Some(ClaudeConnection::new(
                    Some(script),
                    state.test_wake().clone(),
                )));
            })
        });
        let bounds = cx.debug_bounds("welcome-claude").unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.simulate_input("a long one");
        cx.simulate_keystrokes("ctrl-enter");
        wait_until(cx, &view, |view, cx| {
            view.state.read(cx).conversations()[&1]
                .rows
                .iter()
                .any(|row| {
                    matches!(row, Row::Item(item)
                if item.status == ItemStatus::InProgress && item.text.contains("word1"))
                })
        });
        cx.simulate_input("change course");
        cx.simulate_keystrokes("ctrl-enter");
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(300));
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            let conversation = &view.state.read(cx).conversations()[&1];
            let error = conversation.error.clone().unwrap_or_default();
            assert!(
                error.starts_with("This Claude Code version does not report message progress"),
                "{error}"
            );
            assert!(conversation.running, "the turn goes on");
            assert_eq!(view.composer.read(cx).value(), "change course");
            assert!(
                !conversation
                    .rows
                    .iter()
                    .any(|row| matches!(row, Row::Item(item)
                if item.text == "change course"))
            );
        });
        wait_until(cx, &view, |view, cx| {
            !view.state.read(cx).conversations()[&1].running
        });
    }

    #[gpui::test]
    fn menu_created_session_receives_typed_draft(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let store = Store::open(&path).unwrap();
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (_view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        for selector in ["new-session", "new-claude"] {
            let bounds = cx.debug_bounds(selector).expect("control rendered");
            cx.simulate_click(bounds.center(), gpui::Modifiers::none());
            cx.run_until_parked();
        }
        cx.simulate_input("Menu draft");
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(300));
        cx.run_until_parked();
        assert_eq!(
            Store::open(&path).unwrap().workspace().sessions()[0].draft(),
            "Menu draft"
        );
    }

    /// Row actions show on hover, so point at the row before clicking one.
    fn hover(cx: &mut gpui::VisualTestContext, selector: &'static str) {
        let bounds = cx.debug_bounds(selector).expect("row rendered");
        cx.simulate_mouse_move(bounds.center(), None, gpui::Modifiers::none());
        cx.run_until_parked();
    }

    #[gpui::test]
    fn settled_section_starts_collapsed_and_opens_below_working(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store
            .create(Provider::Claude, &working_directory(), 1)
            .unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1)
            .unwrap();
        store.set_settled(1, true).unwrap();
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (_view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        assert!(cx.debug_bounds("session-1").is_none());
        assert!(cx.debug_bounds("session-2").is_some());

        click(cx, "settled-section");
        let y = |cx: &mut gpui::VisualTestContext, selector| {
            cx.debug_bounds(selector).unwrap().origin.y
        };
        let (active, header, settled) = (
            y(cx, "session-2"),
            y(cx, "settled-section"),
            y(cx, "session-1"),
        );
        assert!(active < header && header < settled);
    }

    fn y(cx: &mut gpui::VisualTestContext, selector: &'static str) -> Pixels {
        cx.debug_bounds(selector).expect("rendered").origin.y
    }

    #[gpui::test]
    fn needs_action_holds_approval_failure_and_reply_above_working(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        for now in 1..=5 {
            store
                .create(Provider::Codex, &working_directory(), now)
                .unwrap();
        }
        store.record_turn_end(1, true, 10).unwrap();
        store.record_turn_end(2, false, 11).unwrap();
        store.set_settled(5, true).unwrap();
        store.record_turn_end(5, true, 12).unwrap();
        store.select(4).unwrap();
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        view.update(cx, |this, cx| {
            let mut live = Conversation::default();
            live.running = true;
            live.approvals.push(keystone_agent::Approval {
                request: keystone_agent::RequestId("1".into()),
                item_id: None,
                action: "cargo test".into(),
                detail: None,
            });
            this.state.update(cx, |state, _| {
                state.test_conversations_mut().insert(3, live)
            });
            this.refresh(cx);
        });
        cx.run_until_parked();

        let header = y(cx, "attention-section");
        let rows = [y(cx, "session-1"), y(cx, "session-2"), y(cx, "session-3")];
        let working = y(cx, "working-section");
        assert!(rows.iter().all(|&row| header < row && row < working));
        assert!(working < y(cx, "session-4"));
        assert!(y(cx, "session-4") < y(cx, "settled-section"));
        // A settled session stays in collapsed Settled even with a failed turn.
        assert!(cx.debug_bounds("session-5").is_none());
        view.read_with(cx, |this, cx| {
            let sidebar = this.session_sidebar.read(cx);
            assert!(sidebar.shows_time);
        });
    }

    #[gpui::test]
    fn a_codex_that_fails_to_start_moves_the_session_to_needs_action(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1)
            .unwrap();
        store
            .create(Provider::Codex, &working_directory(), 2)
            .unwrap();
        store.select(1).unwrap();
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        let missing = dir.path().join("no-such-codex");
        view.update_in(cx, |this, window, cx| {
            this.state.update(cx, |state, _| {
                state.test_set_codex(Some(CodexConnection::new(
                    Some(missing),
                    "0",
                    state.test_wake().clone(),
                )));
            });
            this.composer
                .update(cx, |input, cx| input.set_value("hello".into(), cx));
            let draft = this.composer_draft(cx).unwrap();
            this.send_message(draft, window, cx);
        });
        cx.run_until_parked();
        // The start fails on the connection's worker, after the send returned. Until
        // then the turn is running.
        wait_until(cx, &view, |view, cx| {
            view.state.read(cx).conversations()[&1].error.is_some()
                && !view.state.read(cx).conversations()[&1].running
        });
        cx.run_until_parked();
        let header = y(cx, "attention-section");
        assert!(header < y(cx, "session-1") && y(cx, "session-1") < y(cx, "working-section"));
        assert!(y(cx, "working-section") < y(cx, "session-2"));
        view.read_with(cx, |this, cx| {
            let session = &this.state.read(cx).store().workspace().sessions()[0];
            assert_eq!(session.last_turn(), Some(keystone_core::LastTurn::Failed));
            assert_eq!(
                session.attention(false, false, false),
                Some(Attention::Failed)
            );
        });
    }

    #[gpui::test]
    fn an_approval_held_while_history_loads_needs_action_once_it_loads(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1)
            .unwrap();
        // Selected, so session 1 loads only when the test asks for it.
        store
            .create(Provider::Codex, &working_directory(), 2)
            .unwrap();
        let sessions = dir.path().join("sessions");
        seed_long_session(&sessions, 1, 3);
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        // Session 1's history arrives only when the test releases it.
        let (release, released) = std::sync::mpsc::channel::<()>();
        let released = std::sync::Mutex::new(released);
        view.update(cx, |this, cx| {
            let read: keystone_agent::log::Reader = Arc::new(move |dir, session| {
                if session == 1 {
                    let _ = released.lock().unwrap().recv();
                }
                keystone_agent::log::read(dir, session)
            });
            this.state.update(cx, |state, _| {
                state.test_set_logs(Logs::with_reader(
                    sessions.clone(),
                    state.test_wake().clone(),
                    read,
                ));
            });
            this.state
                .update(cx, |state, _| state.test_conversations_mut().clear());
            this.state.update(cx, |state, _| {
                state.test_conversation(2);
            });
            assert!(
                this.state
                    .update(cx, |state, _| state.test_conversation(1).is_loading())
            );
            this.state.update(cx, |state, _| {
                state.test_conversation(1).apply(Event::ApprovalRequested(
                    keystone_agent::Approval {
                        request: keystone_agent::RequestId("1".into()),
                        item_id: None,
                        action: "cargo test".into(),
                        detail: None,
                    },
                ))
            });
            this.refresh(cx);
        });
        cx.run_until_parked();
        // Held with the history, as the approval card is.
        assert!(y(cx, "working-section") < y(cx, "session-1"));

        release.send(()).unwrap();
        wait_until(cx, &view, |view, cx| {
            !view.state.read(cx).conversations()[&1].is_loading()
        });
        cx.run_until_parked();
        view.read_with(cx, |this, cx| {
            assert_eq!(this.state.read(cx).conversations()[&1].approvals.len(), 1)
        });
        let header = y(cx, "attention-section");
        assert!(header < y(cx, "session-1") && y(cx, "session-1") < y(cx, "working-section"));
    }

    #[gpui::test]
    fn selection_and_leaving_do_not_acknowledge_an_unread_reply(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let mut store = Store::open(&path).unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1)
            .unwrap();
        store
            .create(Provider::Codex, &working_directory(), 2)
            .unwrap();
        store.record_turn_end(1, false, 3).unwrap();
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (_view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        assert!(y(cx, "session-1") < y(cx, "working-section"));

        click(cx, "session-1");
        assert!(y(cx, "session-1") < y(cx, "working-section"));

        click(cx, "session-2");
        assert!(y(cx, "session-1") < y(cx, "working-section"));
        assert_eq!(
            Store::open(&path).unwrap().workspace().sessions()[0].attention(false, false, false),
            Some(Attention::Replied)
        );
    }

    /// Types a long single line into a Codex composer, clicks `row` (0-based) at its
    /// left edge, presses `keys`, and types `mark`. Returns the composer's text.
    fn mark_visual_row(
        row: usize,
        keys: &str,
        mark: &str,
        cx: &mut gpui::TestAppContext,
    ) -> String {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1)
            .unwrap();
        cx.update(|cx| {
            keystone_ui::init(cx);
            bind_keys(cx);
        });
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        let line = (0..60)
            .map(|n| format!("w{n:02}"))
            .collect::<Vec<_>>()
            .join(" ");
        click(cx, "composer-field");
        cx.simulate_input(&line);
        let field = cx.debug_bounds("composer-field").unwrap();
        let point = field.origin + gpui::point(px(1.), px(20.) * row + px(10.));
        cx.simulate_click(point, gpui::Modifiers::none());
        if !keys.is_empty() {
            cx.simulate_keystrokes(keys);
        }
        cx.simulate_input(mark);
        cx.run_until_parked();
        view.read_with(cx, |view, cx| view.composer.read(cx).value().to_owned())
    }

    #[gpui::test]
    fn a_long_draft_wraps_and_the_composer_grows_to_its_cap(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1)
            .unwrap();
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        let field = |cx: &mut gpui::VisualTestContext| cx.debug_bounds("composer-field").unwrap();
        let send = |cx: &mut gpui::VisualTestContext| cx.debug_bounds("send").unwrap();
        let (empty, toolbar) = (field(cx), send(cx));
        assert_eq!(empty.size.height, px(44.));

        let set = |text: String, cx: &mut gpui::VisualTestContext| {
            view.update(cx, |this, cx| {
                this.composer
                    .update(cx, |input, cx| input.set_value(text, cx))
            });
            cx.run_until_parked();
        };
        set("word ".repeat(60), cx);
        let wrapped = field(cx);
        assert!(wrapped.size.height > px(44.) && wrapped.size.height < px(160.));
        // The composer grows upward; its toolbar stays docked.
        assert_eq!(wrapped.bottom(), empty.bottom());
        assert_eq!(send(cx), toolbar);
        assert_eq!(wrapped.size.width, empty.size.width);

        // Eight rows at most, then the draft scrolls inside the field.
        set("word ".repeat(600), cx);
        assert_eq!(field(cx).size.height, px(160.));
        assert_eq!(send(cx), toolbar);
    }

    #[gpui::test]
    fn clicks_and_arrows_follow_wrapped_rows(cx: &mut gpui::TestAppContext) {
        let words = |text: &str, mark: &str| {
            let index = text.find(mark).unwrap();
            // A wrapped row starts at a word, after the space the row above ended on.
            assert!(index > 0, "{text}");
            assert_eq!(&text[index - 1..index], " ", "{text}");
            assert!(text[index + mark.len()..].starts_with('w'), "{text}");
            text[..index].split(' ').count() - 1
        };
        let second = words(&mark_visual_row(1, "", "|", cx), "|");
        let third = words(&mark_visual_row(1, "down", "|", cx), "|");
        let back = words(&mark_visual_row(2, "up", "|", cx), "|");
        assert!(second > 0 && third > second, "{second} {third}");
        assert_eq!(back, second);
    }

    #[gpui::test]
    fn row_settle_moves_that_session_and_keeps_the_selection(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let mut store = Store::open(&path).unwrap();
        store
            .create(Provider::Claude, &working_directory(), 1)
            .unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1)
            .unwrap();
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (_view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        let saved = |path: &Path| {
            let store = Store::open(path).unwrap();
            let settled = store
                .workspace()
                .sessions()
                .iter()
                .map(|s| s.settled())
                .collect::<Vec<_>>();
            (store.workspace().selected(), settled)
        };

        hover(cx, "session-1");
        click(cx, "settle-1");
        assert_eq!(saved(&path), (Some(2), vec![true, false]));

        click(cx, "settled-section");
        hover(cx, "session-1");
        click(cx, "settle-1");
        assert_eq!(saved(&path), (Some(2), vec![false, false]));
    }

    #[gpui::test]
    fn sidebar_toggle_hides_the_column_and_ctrl_f_restores_it(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("workspace.json")).unwrap();
        cx.update(|cx| {
            keystone_ui::init(cx);
            bind_keys(cx);
        });
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        let toggle_x =
            |cx: &mut gpui::VisualTestContext| cx.debug_bounds("sidebar-toggle").unwrap().origin.x;
        assert_eq!(toggle_x(cx), px(6.));
        assert!(cx.debug_bounds("search-field").is_some());

        click(cx, "sidebar-toggle");
        assert_eq!(toggle_x(cx), px(6.));

        cx.simulate_keystrokes("ctrl-f");
        assert_eq!(toggle_x(cx), px(6.));
        assert!(cx.debug_bounds("search-field").is_some());
        view.update_in(cx, |this, window, cx| {
            assert!(this.search.focus_handle(cx).is_focused(window))
        });
    }

    #[test]
    fn only_a_turn_the_user_ended_is_interrupted_without_failing() {
        let ended = |status, stopped| Event::TurnCompleted {
            status,
            error: None,
            stopped,
        };
        assert_eq!(
            turn_failed(&ended(TurnStatus::Completed, false)),
            Some(false)
        );
        assert_eq!(
            turn_failed(&ended(TurnStatus::Interrupted, true)),
            Some(false)
        );
        assert_eq!(
            turn_failed(&ended(TurnStatus::Interrupted, false)),
            Some(true)
        );
        assert_eq!(turn_failed(&ended(TurnStatus::Failed, true)), Some(true));
        let failed = Event::Failed {
            message: "Codex exited".into(),
        };
        assert_eq!(turn_failed(&failed), Some(true));
        assert_eq!(turn_failed(&Event::TurnStarted), None);
    }

    /// A Codex driver with session 1's thread open, standing in for the app-server.
    struct Codex {
        driver: keystone_agent::codex::CodexDriver,
        events: Vec<(u64, Event)>,
    }

    impl Codex {
        const THREAD: &str = "thread";

        fn open() -> Self {
            use keystone_agent::{Outputs, codex::CodexDriver};
            let mut out = Outputs::default();
            let mut driver = CodexDriver::new("0", &mut out);
            driver.on_line(br#"{"id":1,"result":{}}"#, &mut out);
            driver.command(
                Command::Open {
                    session: 1,
                    cwd: "/tmp".into(),
                    resume: None,
                    model: None,
                },
                &mut out,
            );
            let thread = format!(
                r#"{{"id":2,"result":{{"thread":{{"id":"{}"}}}}}}"#,
                Self::THREAD
            );
            driver.on_line(thread.as_bytes(), &mut out);
            Self {
                driver,
                events: Vec::new(),
            }
        }

        fn command(&mut self, command: Command) {
            let mut out = keystone_agent::Outputs::default();
            self.driver.command(command, &mut out);
            self.events.extend(out.events);
        }

        fn line(&mut self, line: String) {
            let mut out = keystone_agent::Outputs::default();
            self.driver.on_line(line.as_bytes(), &mut out);
            self.events.extend(out.events);
        }

        fn send(&mut self, turn: &str) {
            self.command(Command::Send {
                session: 1,
                text: turn.into(),
                images: Vec::new(),
            });
            self.line(format!(
                r#"{{"method":"turn/started","params":{{"threadId":"{}","turn":{{"id":"{turn}"}}}}}}"#,
                Self::THREAD
            ));
        }

        fn end(&mut self, turn: &str, status: &str) {
            self.line(format!(
                r#"{{"method":"turn/completed","params":{{"threadId":"{}","turn":{{"id":"{turn}","status":"{status}"}}}}}}"#,
                Self::THREAD
            ));
        }

        fn stop(&mut self) {
            self.command(Command::Interrupt { session: 1 });
        }

        /// Asks to run a command, offering `decisions`, and answers it.
        fn ask(&mut self, request: u64, decisions: &[&str], allow: bool) {
            self.line(format!(
                r#"{{"id":{request},"method":"item/commandExecution/requestApproval","params":{{"threadId":"{}","itemId":"exec","command":"touch x","availableDecisions":{decisions:?}}}}}"#,
                Self::THREAD
            ));
            self.command(Command::Answer {
                session: 1,
                request: keystone_agent::RequestId(request.to_string()),
                allow,
            });
        }
    }

    /// Session 1 with three saved turns whose history loads only when `release` is
    /// sent, and session 2 selected.
    fn held_history(
        cx: &mut gpui::TestAppContext,
    ) -> (
        tempfile::TempDir,
        gpui::Entity<Keystone>,
        &mut gpui::VisualTestContext,
        std::sync::mpsc::Sender<()>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1)
            .unwrap();
        store
            .create(Provider::Codex, &working_directory(), 2)
            .unwrap();
        let sessions = dir.path().join("sessions");
        seed_long_session(&sessions, 1, 3);
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        let (release, released) = std::sync::mpsc::channel::<()>();
        let released = std::sync::Mutex::new(released);
        view.update(cx, |this, cx| {
            let read: keystone_agent::log::Reader = Arc::new(move |dir, session| {
                if session == 1 {
                    let _ = released.lock().unwrap().recv();
                }
                keystone_agent::log::read(dir, session)
            });
            this.state.update(cx, |state, _| {
                state.test_set_logs(Logs::with_reader(
                    sessions.clone(),
                    state.test_wake().clone(),
                    read,
                ));
            });
            this.state
                .update(cx, |state, _| state.test_conversations_mut().clear());
            this.state.update(cx, |state, _| {
                state.test_conversation(2);
            });
            assert!(
                this.state
                    .update(cx, |state, _| state.test_conversation(1).is_loading())
            );
        });
        (dir, view, cx, release)
    }

    fn deliver(view: &gpui::Entity<Keystone>, cx: &mut gpui::VisualTestContext, codex: &mut Codex) {
        let events = std::mem::take(&mut codex.events);
        view.update(cx, |this, cx| this.receive(Vec::new(), events, cx));
    }

    fn load(
        view: &gpui::Entity<Keystone>,
        cx: &mut gpui::VisualTestContext,
        release: &std::sync::mpsc::Sender<()>,
    ) {
        release.send(()).unwrap();
        wait_until(cx, view, |view, cx| {
            !view.state.read(cx).conversations()[&1].is_loading()
        });
    }

    fn last_turn(
        view: &gpui::Entity<Keystone>,
        cx: &mut gpui::VisualTestContext,
    ) -> Option<keystone_core::LastTurn> {
        view.read_with(cx, |this, cx| {
            this.state.read(cx).store().workspace().sessions()[0].last_turn()
        })
    }

    #[gpui::test]
    fn a_stop_while_history_loads_does_not_excuse_the_next_turn(cx: &mut gpui::TestAppContext) {
        let (_dir, view, cx, release) = held_history(cx);
        let mut codex = Codex::open();
        codex.send("a");
        codex.stop();
        codex.end("a", "interrupted");
        deliver(&view, cx, &mut codex);
        assert_eq!(
            last_turn(&view, cx),
            Some(keystone_core::LastTurn::Finished)
        );
        // A second turn is sent before the history arrives, and is cut off by Codex.
        codex.send("b");
        codex.end("b", "interrupted");
        deliver(&view, cx, &mut codex);
        load(&view, cx, &release);
        assert_eq!(last_turn(&view, cx), Some(keystone_core::LastTurn::Failed));
    }

    #[gpui::test]
    fn a_stop_survives_history_replaying_an_earlier_turn(cx: &mut gpui::TestAppContext) {
        let (_dir, view, cx, release) = held_history(cx);
        let mut codex = Codex::open();
        codex.send("a");
        codex.end("a", "completed");
        deliver(&view, cx, &mut codex);
        codex.send("b");
        codex.stop();
        deliver(&view, cx, &mut codex);
        // The history arrives and replays turn a's end before turn b's arrives.
        load(&view, cx, &release);
        codex.end("b", "interrupted");
        deliver(&view, cx, &mut codex);
        assert_eq!(
            last_turn(&view, cx),
            Some(keystone_core::LastTurn::Finished)
        );
    }

    #[gpui::test]
    fn a_denial_excuses_an_interruption_only_when_it_ends_the_turn(cx: &mut gpui::TestAppContext) {
        let (_dir, view, cx, release) = held_history(cx);
        load(&view, cx, &release);
        let mut codex = Codex::open();
        // Denying with "decline" lets the turn go on; a later interruption is a failure,
        // even after another request was allowed.
        codex.send("a");
        codex.ask(10, &["accept", "decline", "cancel"], false);
        codex.ask(11, &["accept", "decline", "cancel"], true);
        codex.end("a", "interrupted");
        deliver(&view, cx, &mut codex);
        assert_eq!(last_turn(&view, cx), Some(keystone_core::LastTurn::Failed));
        // With no "decline" offered, denying cancels the turn.
        codex.send("b");
        codex.ask(12, &["accept", "cancel"], false);
        codex.end("b", "interrupted");
        deliver(&view, cx, &mut codex);
        assert_eq!(
            last_turn(&view, cx),
            Some(keystone_core::LastTurn::Finished)
        );
        // A Stop of one turn and a denial in another stay with their own turns.
        codex.send("c");
        codex.stop();
        codex.end("c", "interrupted");
        codex.send("d");
        codex.ask(13, &["accept", "decline"], false);
        codex.end("d", "interrupted");
        deliver(&view, cx, &mut codex);
        assert_eq!(last_turn(&view, cx), Some(keystone_core::LastTurn::Failed));
    }

    #[gpui::test]
    fn a_late_stop_keeps_the_failure_it_arrived_after(cx: &mut gpui::TestAppContext) {
        let (_dir, view, cx, release) = held_history(cx);
        load(&view, cx, &release);
        let mut codex = Codex::open();
        codex.send("a");
        codex.end("a", "failed");
        deliver(&view, cx, &mut codex);
        let rows = view.read_with(cx, |this, cx| {
            this.state.read(cx).conversations()[&1].rows.len()
        });
        // The user pressed Stop before the failure reached them.
        codex.stop();
        deliver(&view, cx, &mut codex);
        assert_eq!(last_turn(&view, cx), Some(keystone_core::LastTurn::Failed));
        assert_eq!(
            view.read_with(cx, |this, cx| this.state.read(cx).conversations()[&1]
                .rows
                .len()),
            rows
        );
    }

    #[gpui::test]
    fn a_second_press_on_an_answered_approval_leaves_the_next_one_waiting(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        Store::open(&path)
            .unwrap()
            .create(Provider::Codex, &working_directory(), 1)
            .unwrap();
        let (view, cx) = open_window(&path, cx);
        wait_until(cx, &view, |view, cx| {
            view.state
                .read(cx)
                .conversations()
                .get(&1)
                .is_some_and(|c| !c.is_loading())
        });
        let mut codex = Codex::open();
        codex.send("a");
        for (request, command) in [(10, "touch first"), (11, "touch second")] {
            codex.line(format!(
                r#"{{"id":{request},"method":"item/commandExecution/requestApproval","params":{{"threadId":"{}","itemId":"exec-{request}","command":"{command}","availableDecisions":["accept","decline"]}}}}"#,
                Codex::THREAD
            ));
        }
        deliver(&view, cx, &mut codex);
        let allow = cx
            .debug_bounds("approve")
            .expect("approval card rendered")
            .center();
        cx.update(|window, cx| {
            for _ in 0..2 {
                let down = gpui::MouseDownEvent {
                    position: allow,
                    modifiers: gpui::Modifiers::none(),
                    button: gpui::MouseButton::Left,
                    click_count: 1,
                    first_mouse: false,
                };
                let up = gpui::MouseUpEvent {
                    position: allow,
                    modifiers: gpui::Modifiers::none(),
                    button: gpui::MouseButton::Left,
                    click_count: 1,
                };
                window.dispatch_event(gpui::InputEvent::to_platform_input(down), cx);
                window.dispatch_event(gpui::InputEvent::to_platform_input(up), cx);
            }
        });
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            let conversation = &view.state.read(cx).conversations()[&1];
            let waiting: Vec<_> = conversation
                .approvals
                .iter()
                .map(|a| a.action.as_str())
                .collect();
            assert_eq!(waiting, ["touch second"]);
            let decisions: Vec<_> = conversation
                .rows
                .iter()
                .filter(|row| matches!(row, Row::Decision { .. }))
                .collect();
            assert_eq!(
                decisions,
                [&Row::Decision {
                    action: "touch first".into(),
                    allowed: true
                }]
            );
        });
        assert!(
            cx.debug_bounds("approve").is_some(),
            "the next request stays on screen"
        );
    }

    /// A Claude driver for session 1, standing in for the `claude` process.
    struct Claude {
        driver: keystone_agent::claude::ClaudeDriver,
        sent: u64,
        events: Vec<(u64, Event)>,
    }

    impl Claude {
        const SEED: u64 = 7;

        fn open() -> Self {
            let mut out = keystone_agent::Outputs::default();
            let driver = keystone_agent::claude::ClaudeDriver::new(
                1,
                false,
                Self::SEED,
                ApprovalMode::Provider,
                &mut out,
            );
            let mut claude = Self {
                driver,
                sent: 0,
                events: Vec::new(),
            };
            // Claude confirms the mode it launched in, so messages are written.
            claude.line(
                r#"{"type":"control_response","response":{"subtype":"success","request_id":"keystone-1","response":{"current_permission_mode":"default"}}}"#
                    .into(),
            );
            claude.line(
                r#"{"type":"system","subtype":"init","session_id":"s","capabilities":["msg_lifecycle_v1"]}"#
                    .into(),
            );
            claude
        }

        fn line(&mut self, line: String) {
            let mut out = keystone_agent::Outputs::default();
            self.driver.on_line(line.as_bytes(), &mut out);
            self.events.extend(out.events);
        }

        fn lifecycle(&mut self, state: &str) {
            let uuid = keystone_agent::claude::message_uuid(Self::SEED, self.sent);
            self.line(format!(
                r#"{{"type":"command_lifecycle","command_uuid":"{uuid}","state":"{state}"}}"#
            ));
        }

        fn send(&mut self) {
            let mut out = keystone_agent::Outputs::default();
            self.driver.command(
                Command::Send {
                    session: 1,
                    text: "hello".into(),
                    images: Vec::new(),
                },
                &mut out,
            );
            self.events.extend(out.events);
            self.sent += 1;
            self.lifecycle("queued");
            self.lifecycle("started");
        }

        fn stop(&mut self) {
            let mut out = keystone_agent::Outputs::default();
            self.driver
                .command(Command::Interrupt { session: 1 }, &mut out);
            self.events.extend(out.events);
        }

        /// Ends the message with a result, failed or not.
        fn end(&mut self, failed: bool) {
            self.line(if failed {
                r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["overloaded"]}"#.into()
            } else {
                r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#.into()
            });
            self.lifecycle("completed");
        }
    }

    fn deliver_claude(
        view: &gpui::Entity<Keystone>,
        cx: &mut gpui::VisualTestContext,
        claude: &mut Claude,
    ) {
        let events = std::mem::take(&mut claude.events);
        view.update(cx, |this, cx| this.receive(Vec::new(), events, cx));
    }

    #[gpui::test]
    fn a_claude_stop_or_failure_classifies_its_own_turn_while_history_loads(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_dir, view, cx, release) = held_history(cx);
        let mut claude = Claude::open();
        claude.send();
        claude.stop();
        claude.end(true);
        deliver_claude(&view, cx, &mut claude);
        assert_eq!(
            last_turn(&view, cx),
            Some(keystone_core::LastTurn::Finished)
        );
        // The next turn fails on its own before the history arrives.
        claude.send();
        claude.end(true);
        deliver_claude(&view, cx, &mut claude);
        load(&view, cx, &release);
        assert_eq!(last_turn(&view, cx), Some(keystone_core::LastTurn::Failed));
        // A Stop that reaches Claude after the failure changes nothing.
        let rows = view.read_with(cx, |this, cx| {
            this.state.read(cx).conversations()[&1].rows.len()
        });
        claude.stop();
        deliver_claude(&view, cx, &mut claude);
        assert_eq!(last_turn(&view, cx), Some(keystone_core::LastTurn::Failed));
        assert_eq!(
            view.read_with(cx, |this, cx| this.state.read(cx).conversations()[&1]
                .rows
                .len()),
            rows
        );
    }

    /// A wrapper that runs the scripted Claude with `env` set, so tests sharing this
    /// process see no change.
    fn claude_script(dir: &std::path::Path, env: &str) -> PathBuf {
        let script = dir.join("claude");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n{env} KEYSTONE_FAKE_CLAUDE_DELAY_MS=5 exec '{}' \"$@\"\n",
                fake("keystone-fake-claude").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        script
    }

    fn in_section(cx: &mut gpui::VisualTestContext, row: &'static str, section: Section) -> bool {
        let row = y(cx, row);
        let mut after = |header| cx.debug_bounds(header).map(|b| b.origin.y);
        let (start, end) = match section {
            Section::Attention => (after("attention-section"), after("working-section")),
            Section::Working => (after("working-section"), after("settled-section")),
            Section::Settled => (after("settled-section"), None),
        };
        start.is_some_and(|start| start < row) && end.is_none_or(|end| row < end)
    }

    /// A scripted agent shows a checklist: it waits in Needs action, settling does not
    /// answer it, a tick writes its data model, and the press reaches the agent as the
    /// tool's result.
    fn widget_round_trip(provider: Provider, cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store.create(provider, &working_directory(), 1).unwrap();
        store.select(1).unwrap();
        let program = match provider {
            Provider::Claude => claude_script(dir.path(), ""),
            Provider::Codex => wrapper(
                dir.path(),
                "keystone-fake-codex",
                "KEYSTONE_FAKE_CODEX_DELAY_MS=2 KEYSTONE_FAKE_CODEX_VERSION=0.163.0",
            ),
        };
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        view.update(cx, |this, cx| {
            this.state.update(cx, |state, _| match provider {
                Provider::Claude => state.test_set_claude(Some(ClaudeConnection::new(
                    Some(program),
                    state.test_wake().clone(),
                ))),
                Provider::Codex => state.test_set_codex(Some(CodexConnection::new(
                    Some(program),
                    "test",
                    state.test_wake().clone(),
                ))),
            })
        });
        let widget = |view: &Keystone, cx: &App| {
            view.state.read(cx).conversations()[&1]
                .rows
                .iter()
                .enumerate()
                .find_map(|(index, row)| match row {
                    Row::Surface(surface) => Some((index, surface.clone())),
                    _ => None,
                })
        };

        send_text(&view, cx, "show a widget checklist");
        wait_until(cx, &view, |view, cx| {
            widget(view, cx).is_some_and(|(_, surface)| surface.waiting())
        });
        cx.run_until_parked();
        assert!(in_section(cx, "session-1", Section::Attention));
        view.update(cx, |this, cx| this.settle(1, true, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(
                view.error.as_deref(),
                Some("Answer the widget before settling this session.")
            );
            assert!(!view.state.read(cx).store().workspace().sessions()[0].settled());
            assert!(
                widget(view, cx).unwrap().1.waiting(),
                "settling never answers it"
            );
        });

        let index = view.read_with(cx, |view, cx| widget(view, cx).unwrap().0);
        click(cx, format!("widget-{index}-tests-unchecked").leak());
        wait_until(cx, &view, |view, cx| {
            widget(view, cx).unwrap().1.value("/items/tests") == Some(&serde_json::json!(true))
        });
        click(cx, format!("widget-{index}-send-button").leak());
        wait_until(cx, &view, |view, cx| {
            reply_with(view, "Widget results", cx).is_some()
        });
        view.read_with(cx, |view, cx| {
            let reply = reply_with(view, "Widget results", cx).unwrap();
            assert!(
                reply.contains(
                    r#""context":{"items":{"changelog":false,"docs":true,"tests":true}}"#
                ),
                "{reply}"
            );
            let (_, surface) = widget(view, cx).unwrap();
            assert_eq!(surface.status, keystone_agent::surface::Status::Answered);
            assert_eq!(surface.last_action.as_deref(), Some("submit"));
        });
    }

    /// After the turn that asked is stopped, a press goes as a new message that names
    /// the call, and the draft stays.
    #[gpui::test]
    fn a_press_after_its_turn_ends_is_sent_as_a_message(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store
            .create(Provider::Claude, &working_directory(), 1)
            .unwrap();
        store.select(1).unwrap();
        let script = claude_script(dir.path(), "");
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        view.update(cx, |this, cx| {
            this.state.update(cx, |state, _| {
                state.test_set_claude(Some(ClaudeConnection::new(
                    Some(script),
                    state.test_wake().clone(),
                )))
            })
        });
        let widget = |view: &Keystone, cx: &App| {
            view.state.read(cx).conversations()[&1]
                .rows
                .iter()
                .enumerate()
                .find_map(|(index, row)| match row {
                    Row::Surface(surface) => Some((index, surface.clone())),
                    _ => None,
                })
        };
        send_text(&view, cx, "show a widget checklist");
        wait_until(cx, &view, |view, cx| {
            widget(view, cx).is_some_and(|(_, surface)| surface.waiting())
        });
        view.update_in(cx, |this, window, cx| this.stop_turn(1, window, cx));
        wait_until(cx, &view, |view, cx| {
            !view.state.read(cx).conversations()[&1].running
        });
        let (index, surface) = view.read_with(cx, |view, cx| widget(view, cx).unwrap());
        assert_eq!(surface.status, keystone_agent::surface::Status::Stopped);
        view.update(cx, |view, cx| {
            view.composer
                .update(cx, |input, cx| input.set_value("my draft".into(), cx))
        });
        click(cx, format!("widget-{index}-skip-button").leak());
        wait_until(cx, &view, |view, cx| {
            reply_with(view, "Keystone widget action", cx).is_some()
        });
        view.read_with(cx, |view, cx| {
            let rows = &view.state.read(cx).conversations()[&1].rows;
            assert!(rows.iter().any(|row| matches!(row, Row::Item(item)
                if item.kind == ItemKind::UserMessage
                    && item.text.contains(&format!("show_ui call {}", surface.call))
                    && item.text.contains(r#""name":"skip""#))));
            assert_eq!(view.composer.read(cx).value(), "my draft");
        });
    }

    /// A click in a widget's text field focuses the field, not the conversation, so
    /// what the user types lands in the widget's data model.
    #[gpui::test]
    fn typing_in_a_widget_text_field_writes_its_value(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1)
            .unwrap();
        store.select(1).unwrap();
        let codex = wrapper(
            dir.path(),
            "keystone-fake-codex",
            "KEYSTONE_FAKE_CODEX_DELAY_MS=2 KEYSTONE_FAKE_CODEX_VERSION=0.163.0",
        );
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        view.update(cx, |this, cx| {
            this.state.update(cx, |state, _| {
                state.test_set_codex(Some(CodexConnection::new(
                    Some(codex),
                    "test",
                    state.test_wake().clone(),
                )))
            })
        });
        let widget = |view: &Keystone, cx: &App| {
            view.state.read(cx).conversations()[&1]
                .rows
                .iter()
                .enumerate()
                .find_map(|(index, row)| match row {
                    Row::Surface(surface) => Some((index, surface.clone())),
                    _ => None,
                })
        };
        send_text(&view, cx, "show a widget form");
        wait_until(cx, &view, |view, cx| {
            widget(view, cx).is_some_and(|(_, surface)| surface.waiting())
        });
        let index = view.read_with(cx, |view, cx| widget(view, cx).unwrap().0);
        click(cx, format!("widget-{index}-summary-field").leak());
        cx.simulate_input("Sidebar flickers");
        wait_until(cx, &view, |view, cx| {
            widget(view, cx).unwrap().1.value("/summary")
                == Some(&serde_json::json!("Sidebar flickers"))
        });
        view.read_with(cx, |view, cx| {
            assert_eq!(
                view.composer.read(cx).value(),
                "",
                "nothing went to the composer"
            );
        });

        // A change from elsewhere, such as another view of the session, reaches the
        // field once it is not being typed in, so its next keystroke builds on it.
        click(cx, "composer-field");
        let call = view.read_with(cx, |view, cx| widget(view, cx).unwrap().1.call);
        view.update(cx, |view, cx| {
            view.state.update(cx, |state, cx| {
                state.edit_widget(1, &call, "/summary", serde_json::json!("Changed"), cx)
            })
        });
        cx.run_until_parked();
        click(cx, format!("widget-{index}-summary-field").leak());
        cx.simulate_input("!");
        wait_until(cx, &view, |view, cx| {
            widget(view, cx).unwrap().1.value("/summary") == Some(&serde_json::json!("Changed!"))
        });
    }

    #[gpui::test]
    fn a_claude_widget_round_trips_its_answer(cx: &mut gpui::TestAppContext) {
        widget_round_trip(Provider::Claude, cx);
    }

    #[gpui::test]
    fn a_codex_widget_round_trips_its_answer(cx: &mut gpui::TestAppContext) {
        widget_round_trip(Provider::Codex, cx);
    }

    #[gpui::test]
    fn claude_sessions_move_between_working_and_needs_action(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store
            .create(Provider::Claude, &working_directory(), 1)
            .unwrap();
        store
            .create(Provider::Claude, &working_directory(), 2)
            .unwrap();
        store.select(1).unwrap();
        let script = claude_script(dir.path(), "");
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        view.update(cx, |this, cx| {
            this.state.update(cx, |state, _| {
                state.test_set_claude(Some(ClaudeConnection::new(
                    Some(script),
                    state.test_wake().clone(),
                )));
            })
        });
        let running = |view: &Keystone, cx: &App| view.state.read(cx).conversations()[&1].running;

        send_text(&view, cx, "a long one");
        assert!(in_section(cx, "session-1", Section::Working));
        wait_until(cx, &view, |view, cx| {
            view.state.read(cx).conversations()[&1]
                .rows
                .iter()
                .any(|row| {
                    matches!(row, Row::Item(item)
                if item.status == ItemStatus::InProgress && item.text.contains("word1"))
                })
        });
        view.update_in(cx, |this, window, cx| this.stop_turn(1, window, cx));
        wait_until(cx, &view, |view, cx| !running(view, cx));
        assert_eq!(
            last_turn(&view, cx),
            Some(keystone_core::LastTurn::Finished)
        );
        assert!(in_section(cx, "session-1", Section::Attention));

        send_text(&view, cx, "please run it");
        wait_until(cx, &view, |view, cx| {
            !view.state.read(cx).conversations()[&1].approvals.is_empty()
        });
        cx.run_until_parked();
        assert!(in_section(cx, "session-1", Section::Attention));
        click(cx, "approve");
        wait_until(cx, &view, |view, cx| !running(view, cx));
        cx.run_until_parked();
        assert!(in_section(cx, "session-1", Section::Attention));

        // A reply that ends while the user is elsewhere is unread.
        send_text(&view, cx, "hello");
        view.update(cx, |this, cx| this.select(2, cx));
        wait_until(cx, &view, |view, cx| !running(view, cx));
        cx.run_until_parked();
        assert!(in_section(cx, "session-1", Section::Attention));
        view.read_with(cx, |this, cx| {
            let session = &this.state.read(cx).store().workspace().sessions()[0];
            assert_eq!(
                session.attention(false, false, false),
                Some(Attention::Replied)
            );
        });
    }

    #[gpui::test]
    fn a_failed_claude_turn_moves_the_session_to_needs_action(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store
            .create(Provider::Claude, &working_directory(), 1)
            .unwrap();
        store
            .create(Provider::Claude, &working_directory(), 2)
            .unwrap();
        store.select(1).unwrap();
        let script = claude_script(dir.path(), "KEYSTONE_FAKE_CLAUDE_SIGNED_OUT=1");
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        view.update(cx, |this, cx| {
            this.state.update(cx, |state, _| {
                state.test_set_claude(Some(ClaudeConnection::new(
                    Some(script),
                    state.test_wake().clone(),
                )));
            })
        });
        send_text(&view, cx, "hello");
        wait_until(cx, &view, |view, cx| {
            !view.state.read(cx).conversations()[&1].running
        });
        cx.run_until_parked();
        assert!(in_section(cx, "session-1", Section::Attention));
        assert!(in_section(cx, "session-2", Section::Working));
        assert_eq!(last_turn(&view, cx), Some(keystone_core::LastTurn::Failed));
    }

    #[gpui::test]
    fn a_claude_that_fails_to_start_moves_the_session_to_needs_action(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        store
            .create(Provider::Claude, &working_directory(), 1)
            .unwrap();
        store
            .create(Provider::Claude, &working_directory(), 2)
            .unwrap();
        store.select(1).unwrap();
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        let missing = dir.path().join("no-such-claude");
        view.update(cx, |this, cx| {
            this.state.update(cx, |state, _| {
                state.test_set_claude(Some(ClaudeConnection::new(
                    Some(missing),
                    state.test_wake().clone(),
                )));
            })
        });
        send_text(&view, cx, "hello");
        wait_until(cx, &view, |view, cx| {
            view.state.read(cx).conversations()[&1].error.is_some()
                && !view.state.read(cx).conversations()[&1].running
        });
        cx.run_until_parked();
        assert!(in_section(cx, "session-1", Section::Attention));
        assert!(in_section(cx, "session-2", Section::Working));
        assert_eq!(last_turn(&view, cx), Some(keystone_core::LastTurn::Failed));
    }

    #[test]
    fn excerpt_is_the_first_written_line_or_the_provider() {
        assert_eq!(
            excerpt(
                "\n  Fix persistence durability first \nthen the rest",
                Provider::Claude
            ),
            "Fix persistence durability first"
        );
        assert_eq!(excerpt(" \n\n", Provider::Codex), "Codex");
        assert_eq!(excerpt("", Provider::Claude), "Claude Code");
    }

    #[gpui::test]
    fn clicking_create_and_settle_persists_sessions(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let store = Store::open(&path).unwrap();
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (_view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        for selector in [
            "new-session",
            "new-claude",
            "new-session",
            "new-codex",
            "settle-2",
        ] {
            let bounds = cx.debug_bounds(selector).expect("control rendered");
            cx.simulate_click(bounds.center(), gpui::Modifiers::none());
            cx.run_until_parked();
        }
        let restored = Store::open(&path).unwrap();
        assert_eq!(restored.workspace().selected(), Some(2));
        assert_eq!(
            restored
                .workspace()
                .sessions()
                .iter()
                .map(|s| (s.provider(), s.settled()))
                .collect::<Vec<_>>(),
            vec![(Provider::Claude, false), (Provider::Codex, true)]
        );
        for selector in ["settled-section", "settle-2"] {
            let bounds = cx.debug_bounds(selector).expect("control rendered");
            cx.simulate_click(bounds.center(), gpui::Modifiers::none());
            cx.run_until_parked();
        }
        let restored = Store::open(path).unwrap();
        assert_eq!(restored.workspace().selected(), Some(2));
        assert!(!restored.workspace().sessions()[1].settled());
    }

    #[gpui::test]
    fn invalid_creation_directory_keeps_selection_and_unsaved_draft(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let (view, cx) = open_window(&path, cx);
        click(cx, "new-session");
        click(cx, "new-codex");
        let before = Store::open(&path).unwrap().saved_snapshot().unwrap();
        let unavailable = dir.path().join("unavailable");
        view.update(cx, |this, cx| {
            this.presentation.project =
                keystone_core::ProjectId::new(&unavailable).map_err(|e| e.to_string());
            this.composer.update(cx, |input, cx| {
                input.set_value("Keep this draft".into(), cx)
            });
            this.create(Provider::Claude, None, cx);
            assert!(this.error.is_some());
            assert_eq!(this.state.read(cx).store().workspace().selected(), Some(1));
            assert_eq!(this.state.read(cx).store().workspace().sessions().len(), 1);
            assert_eq!(this.composer.read(cx).value(), "Keep this draft");
        });
        assert_eq!(
            Store::open(&path).unwrap().saved_snapshot().unwrap(),
            before
        );
    }

    #[gpui::test]
    fn a_rename_saves_on_enter_and_escape_keeps_the_title(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let (view, cx) = open_window(&path, cx);
        click(cx, "new-session");
        click(cx, "new-codex");
        let saved_title = || {
            Store::open(&path).unwrap().workspace().sessions()[0]
                .title()
                .to_owned()
        };

        click(cx, "rename-session");
        view.update(cx, |view, cx| {
            assert!(view.renaming.is_some(), "the title opens the rename dialog");
            view.rename_input.update(cx, |input, cx| {
                input.set_value(" Release  notes ".into(), cx)
            });
        });
        cx.simulate_keystrokes("enter");
        assert!(view.read_with(cx, |view, _cx| view.renaming.is_none()));
        assert_eq!(saved_title(), "Release notes");

        cx.simulate_keystrokes("f2");
        view.update(cx, |view, cx| {
            assert!(view.renaming.is_some(), "F2 opens the rename dialog");
            view.rename_input
                .update(cx, |input, cx| input.set_value("Discarded".into(), cx));
        });
        cx.simulate_keystrokes("escape");
        assert!(view.read_with(cx, |view, _cx| view.renaming.is_none()));
        assert_eq!(saved_title(), "Release notes");
    }

    #[gpui::test]
    fn a_rename_tabs_through_its_field_and_retries_a_failed_save(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        std::fs::create_dir(&state_dir).unwrap();
        let path = state_dir.join("workspace.json");
        let (view, cx) = open_window(&path, cx);
        click(cx, "new-session");
        click(cx, "new-codex");
        let value = |cx: &mut gpui::VisualTestContext| {
            view.read_with(cx, |view, cx| view.rename_input.read(cx).value().to_owned())
        };

        cx.simulate_keystrokes("f2");
        cx.simulate_input("New");
        assert_eq!(
            value(cx),
            "New",
            "the field opens focused with its title selected"
        );
        cx.simulate_keystrokes("tab tab tab");
        cx.simulate_input("er");
        assert_eq!(
            value(cx),
            "Newer",
            "Tab cycles Cancel, Rename, and back to the field"
        );

        std::fs::rename(&state_dir, dir.path().join("backup")).unwrap();
        cx.simulate_keystrokes("shift-tab enter");
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("enter").unwrap(),
        });
        cx.run_until_parked();
        assert!(view.read_with(cx, |view, _cx| view.renaming.is_some()));
        assert!(
            cx.debug_bounds("rename-error").is_some(),
            "the dialog shows the error"
        );
        cx.simulate_input("!");
        assert_eq!(
            value(cx),
            "Newer!",
            "the typed title stays, with the field focused"
        );

        std::fs::rename(dir.path().join("backup"), &state_dir).unwrap();
        cx.simulate_keystrokes("enter");
        assert!(view.read_with(cx, |view, _cx| view.renaming.is_none()));
        let saved = Store::open(&path).unwrap().workspace().sessions()[0]
            .title()
            .to_owned();
        assert_eq!(saved, "Newer!");
    }

    #[gpui::test]
    fn unsaved_draft_survives_failed_navigation_and_retries(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        std::fs::create_dir(&state_dir).unwrap();
        let path = state_dir.join("workspace.json");
        let mut store = Store::open(&path).unwrap();
        store
            .create(Provider::Claude, &working_directory(), 1)
            .unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1)
            .unwrap();
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        std::fs::rename(&state_dir, dir.path().join("backup")).unwrap();
        let bounds = cx.debug_bounds("composer-field").unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.simulate_input("Keep my unsaved draft");
        cx.run_until_parked();
        let bounds = cx.debug_bounds("session-1").unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.state.read(cx).store().workspace().selected(), Some(2));
            assert_eq!(view.composer.read(cx).value(), "Keep my unsaved draft");
            assert!(view.error.as_deref().unwrap().contains("Draft not saved"));
        });
        view.update(cx, |view, cx| {
            view.create(Provider::Claude, None, cx);
            assert_eq!(view.state.read(cx).store().workspace().selected(), Some(2));
            assert_eq!(view.state.read(cx).store().workspace().sessions().len(), 2);
            assert_eq!(view.composer.read(cx).value(), "Keep my unsaved draft");
            assert!(view.error.as_deref().unwrap().contains("Draft not saved"));
        });
        std::fs::rename(dir.path().join("backup"), &state_dir).unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        let restored = Store::open(&path).unwrap();
        assert_eq!(restored.workspace().selected(), Some(1));
        assert_eq!(
            restored.workspace().sessions()[1].draft(),
            "Keep my unsaved draft"
        );
        let bounds = cx.debug_bounds("composer-field").unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.update(|_, cx| {
            TextInput::bind_keys(cx);
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                "first line\nsecond line".into(),
            ));
        });
        cx.simulate_keystrokes("ctrl-v");
        cx.run_until_parked();
        let bounds = cx.debug_bounds("session-2").unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            Store::open(&path).unwrap().workspace().sessions()[0].draft(),
            "first line\nsecond line"
        );
    }

    /// A window over a saved workspace with one selected Codex session, focused the
    /// way `main` focuses it.
    mod handoff {
        use super::*;
        include!("handoff_tests.rs");
    }

    fn open_with_session<'a>(
        dir: &Path,
        cx: &'a mut gpui::TestAppContext,
    ) -> (Entity<Keystone>, &'a mut gpui::VisualTestContext) {
        let path = dir.join("workspace.json");
        let mut store = Store::open(&path).unwrap();
        store
            .create(Provider::Codex, &working_directory(), 1)
            .unwrap();
        drop(store);
        let (view, cx) = open_window(&path, cx);
        view.update_in(cx, |this, window, cx| this.focus.focus(window, cx));
        cx.run_until_parked();
        (view, cx)
    }

    fn run_from_palette(cx: &mut gpui::VisualTestContext, query: &str) {
        cx.simulate_keystrokes("ctrl-k");
        cx.run_until_parked();
        assert!(cx.debug_bounds("palette").is_some(), "the palette opens");
        cx.simulate_input(query);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
    }

    fn palette_open(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext) -> bool {
        view.read_with(cx, |view, _cx| view.palette.is_some())
    }

    #[gpui::test]
    fn palette_new_session_creates_session(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = open_window(&dir.path().join("workspace.json"), cx);
        view.update_in(cx, |this, window, cx| this.focus.focus(window, cx));
        run_from_palette(cx, "new c");
        assert!(!palette_open(&view, cx));
        assert!(cx.debug_bounds("palette").is_none());
        view.read_with(cx, |view, cx| {
            let sessions = view.state.read(cx).store().workspace().sessions();
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].provider(), Provider::Codex);
            assert_eq!(
                view.state.read(cx).store().workspace().selected(),
                Some(sessions[0].id())
            );
        });
        assert!(
            composer_focused(&view, cx),
            "a new session focuses its composer"
        );
    }

    #[gpui::test]
    fn palette_escape_restores_focus(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = open_with_session(dir.path(), cx);
        cx.simulate_keystrokes("ctrl-l");
        cx.simulate_input("half a thought");
        cx.run_until_parked();
        assert!(composer_focused(&view, cx));

        for open in ["ctrl-k", "ctrl-shift-p"] {
            cx.simulate_keystrokes(open);
            cx.run_until_parked();
            assert!(palette_open(&view, cx), "{open} opens the palette");
            assert!(
                !composer_focused(&view, cx),
                "{open}: the palette input has focus"
            );
            cx.simulate_input("sett");
            cx.simulate_keystrokes("escape");
            cx.run_until_parked();
            assert!(!palette_open(&view, cx), "{open}: escape closes it");
            assert!(
                composer_focused(&view, cx),
                "{open}: focus returns to the composer"
            );
            view.read_with(cx, |view, cx| {
                assert_eq!(view.composer.read(cx).value(), "half a thought");
                assert!(!view.state.read(cx).store().workspace().sessions()[0].settled());
            });
        }
    }

    #[gpui::test]
    fn palette_escape_leaves_customize_open(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = open_with_session(dir.path(), cx);
        let customizing = |view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext| {
            view.read_with(cx, |view, _cx| {
                view.presentation.current.customization.is_open()
            })
        };
        cx.simulate_keystrokes("ctrl-shift-l");
        cx.run_until_parked();
        assert!(customizing(&view, cx));
        cx.simulate_keystrokes("ctrl-k");
        cx.run_until_parked();
        assert!(palette_open(&view, cx));
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(!palette_open(&view, cx));
        assert!(customizing(&view, cx), "escape closes only the palette");
    }

    #[gpui::test]
    fn palette_blocked_by_dialog(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = open_with_session(dir.path(), cx);
        cx.simulate_keystrokes("f2");
        cx.run_until_parked();
        assert!(view.read_with(cx, |view, _cx| view.renaming.is_some()));
        cx.simulate_keystrokes("ctrl-k");
        cx.run_until_parked();
        assert!(
            !palette_open(&view, cx),
            "the rename dialog keeps the palette closed"
        );
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        pick_mode(cx, ApprovalMode::FullAccess);
        assert!(asking(&view, cx));
        cx.simulate_keystrokes("ctrl-k");
        cx.run_until_parked();
        assert!(
            !palette_open(&view, cx),
            "the full-access dialog keeps the palette closed"
        );
    }

    #[gpui::test]
    fn palette_runs_toggle_sidebar(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = open_with_session(dir.path(), cx);
        assert!(cx.debug_bounds("session-1").is_some());
        run_from_palette(cx, "toggle sidebar");
        assert!(!palette_open(&view, cx));
        assert!(cx.debug_bounds("session-1").is_none(), "the sidebar hides");
        cx.simulate_keystrokes("ctrl-b");
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("session-1").is_some(),
            "Ctrl+B shows it again"
        );
    }

    #[gpui::test]
    fn palette_runs_toggle_files(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "").unwrap();
        let (view, cx) = open_with_session(dir.path(), cx);
        let saved_files_views = |view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext| {
            view.read_with(cx, |view, cx| {
                view.saved_layout(cx).map(|layout| {
                    layout
                        .shown()
                        .views()
                        .filter(|(_, c)| *c == keystone_core::Component::Files)
                        .count()
                })
            })
        };
        assert_eq!(saved_files_views(&view, cx), None, "no project layout yet");
        run_from_palette(cx, "toggle files");
        assert!(
            cx.debug_bounds("files-tree-row-1").is_some(),
            "Files opens beside the conversation"
        );
        assert!(cx.debug_bounds("conversation").is_some());
        assert_eq!(saved_files_views(&view, cx), Some(1), "the layout is saved");
        cx.simulate_keystrokes("ctrl-shift-e");
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("files-tree-row-1").is_none(),
            "Ctrl+Shift+E closes Files"
        );
        assert_eq!(saved_files_views(&view, cx), Some(0));
    }

    #[gpui::test]
    fn surface_menu_adds_a_conversation_tab_beside_files(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "").unwrap();
        let (_view, cx) = open_with_session(dir.path(), cx);
        cx.simulate_keystrokes("ctrl-shift-e");
        cx.run_until_parked();
        assert!(cx.debug_bounds("files-tab-0").is_some(), "a lone Files tab");
        assert!(cx.debug_bounds("surface-add-files").is_none());

        click(cx, "surface-add");
        assert!(
            cx.debug_bounds("surface-add-files").is_none(),
            "the only Files view is already open"
        );
        click(cx, "surface-add-conversation");
        assert!(cx.debug_bounds("files-tab-0").is_none(), "no longer alone");
        assert!(
            cx.debug_bounds("conversation-view-4").is_some(),
            "the new conversation is the selected tab"
        );
        assert!(cx.debug_bounds("files-tree-row-1").is_none());
        let strip = cx
            .debug_bounds("surface-tabs-3-tab-0")
            .expect("the Files tab is first in its group");
        cx.simulate_click(strip.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("files-tree-row-1").is_some(),
            "Files tab shows again"
        );
        assert!(
            cx.debug_bounds("conversation").is_some(),
            "the main conversation is untouched"
        );

        click(cx, "surface-tabs-3-tab-1");
        click(cx, "surface-tabs-3-tab-1-close");
        assert!(cx.debug_bounds("conversation-view-4").is_none());
        assert!(
            cx.debug_bounds("files-tab-0").is_some(),
            "Files is alone again"
        );
    }

    #[gpui::test]
    fn palette_runs_focus_search(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = open_with_session(dir.path(), cx);
        run_from_palette(cx, "focus search");
        cx.simulate_input("codex");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.search.read(cx).value(), "codex");
        });
    }

    #[gpui::test]
    fn palette_keeps_app_shortcuts_inert(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = open_with_session(dir.path(), cx);
        cx.simulate_keystrokes("ctrl-l");
        cx.simulate_input("half a thought");
        cx.run_until_parked();
        cx.simulate_keystrokes("ctrl-k");
        cx.run_until_parked();
        cx.simulate_keystrokes("ctrl-n");
        cx.simulate_keystrokes("ctrl-l");
        cx.simulate_keystrokes("ctrl-enter");
        cx.simulate_input("tog");
        cx.run_until_parked();
        assert!(palette_open(&view, cx));
        view.read_with(cx, |view, cx| {
            assert_eq!(view.state.read(cx).store().workspace().sessions().len(), 1);
            assert_eq!(
                view.composer.read(cx).value(),
                "half a thought",
                "nothing sent"
            );
            let state = view.palette.as_ref().unwrap().state.read(cx);
            assert_eq!(state.query(cx), "tog", "typing still reaches the palette");
        });
    }

    #[gpui::test]
    fn palette_tab_moves_the_highlight(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = open_with_session(dir.path(), cx);
        cx.simulate_keystrokes("ctrl-k");
        cx.run_until_parked();
        let highlighted = |view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext| {
            view.read_with(cx, |view, cx| {
                let state = view.palette.as_ref().unwrap().state.read(cx);
                (
                    state.highlighted_item(cx).map(|item| item.id.to_string()),
                    state.query(cx).to_owned(),
                )
            })
        };
        cx.simulate_keystrokes("tab tab");
        cx.run_until_parked();
        assert_eq!(
            highlighted(&view, cx),
            (Some("toggle-sidebar".into()), String::new())
        );
        cx.simulate_keystrokes("shift-tab");
        cx.run_until_parked();
        assert_eq!(
            highlighted(&view, cx),
            (Some("new-claude".into()), String::new())
        );
        assert!(palette_open(&view, cx));
    }

    #[gpui::test]
    fn palette_keeps_customize_shortcuts_inert(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = open_with_session(dir.path(), cx);
        cx.simulate_keystrokes("ctrl-shift-l");
        cx.run_until_parked();
        cx.simulate_keystrokes("ctrl-k");
        cx.run_until_parked();
        cx.simulate_keystrokes("ctrl-s");
        cx.run_until_parked();
        assert!(palette_open(&view, cx));
        assert!(
            view.read_with(cx, |view, _cx| view
                .presentation
                .current
                .customization
                .is_open()),
            "Ctrl+S does not apply the layout under the palette"
        );
    }

    fn await_approval(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext) {
        view.update(cx, |this, cx| {
            let mut live = Conversation::default();
            live.running = true;
            live.approvals.push(keystone_agent::Approval {
                request: keystone_agent::RequestId("1".into()),
                item_id: None,
                action: "cargo test".into(),
                detail: None,
            });
            this.state.update(cx, |state, _| {
                state.test_conversations_mut().insert(1, live)
            });
            this.refresh(cx);
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn settle_skips_approval_held_while_history_loads(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = open_with_session(dir.path(), cx);
        view.update(cx, |this, cx| {
            let mut live = Conversation::loading();
            live.apply(keystone_agent::Event::ApprovalRequested(
                keystone_agent::Approval {
                    request: keystone_agent::RequestId("1".into()),
                    item_id: None,
                    action: "cargo test".into(),
                    detail: None,
                },
            ));
            this.state.update(cx, |state, _| {
                state.test_conversations_mut().insert(1, live)
            });
        });
        cx.simulate_keystrokes("ctrl-shift-s");
        cx.run_until_parked();
        assert_eq!(settled(&view, cx), (false, false));
    }

    fn settled(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext) -> (bool, bool) {
        view.read_with(cx, |view, cx| {
            let on_disk = Store::open(view.state.read(cx).store().path()).unwrap();
            (
                view.state.read(cx).store().workspace().sessions()[0].settled(),
                on_disk.workspace().sessions()[0].settled(),
            )
        })
    }

    #[gpui::test]
    fn palette_settle_skips_session_awaiting_approval(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = open_with_session(dir.path(), cx);
        await_approval(&view, cx);
        run_from_palette(cx, "settle");
        assert!(!palette_open(&view, cx));
        assert_eq!(settled(&view, cx), (false, false));
        view.read_with(cx, |view, _cx| {
            assert_eq!(
                view.error.as_deref(),
                Some("Answer the approval request before settling this session.")
            );
        });
    }

    #[gpui::test]
    fn settle_shortcut_skips_session_awaiting_approval(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = open_with_session(dir.path(), cx);
        await_approval(&view, cx);
        cx.simulate_keystrokes("ctrl-shift-s");
        cx.run_until_parked();
        assert_eq!(settled(&view, cx), (false, false));
        assert!(view.read_with(cx, |view, _cx| view.error.is_some()));

        view.update(cx, |this, cx| {
            this.state.update(cx, |state, _| {
                state
                    .test_conversations_mut()
                    .get_mut(&1)
                    .unwrap()
                    .approvals
                    .clear()
            })
        });
        cx.simulate_keystrokes("ctrl-shift-s");
        cx.run_until_parked();
        assert_eq!(
            settled(&view, cx),
            (true, true),
            "settles once nothing waits"
        );
    }

    #[gpui::test]
    #[ignore = "benchmark; run through scripts/perf.py"]
    fn perf_palette_open(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        for index in 0..200 {
            let provider = if index % 2 == 0 {
                Provider::Claude
            } else {
                Provider::Codex
            };
            store.create(provider, &working_directory(), 1).unwrap();
        }
        cx.update(keystone_ui::init);
        cx.executor().allow_parking();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Keystone::new(
                cx.new(|cx| crate::AppState::new(store, cx)),
                false,
                window,
                cx,
            )
        });
        let mut frames_ms = Vec::new();
        for _ in 0..30 {
            view.update_in(cx, |this, window, cx| this.toggle_palette(window, cx));
            cx.update(|window, cx| {
                let start = std::time::Instant::now();
                window.draw(cx).clear(cx);
                frames_ms.push(start.elapsed().as_secs_f64() * 1000.);
            });
            assert!(
                cx.debug_bounds("palette").is_some(),
                "the timed frame drew it"
            );
            view.update_in(cx, |this, window, cx| this.toggle_palette(window, cx));
            cx.update(|window, cx| window.draw(cx).clear(cx));
        }
        assert!(view.read_with(cx, |view, _cx| view.palette.is_none()));
        frames_ms.sort_by(f64::total_cmp);
        eprintln!(
            "PERF palette_open_ms={:.2} palette_open_p95_ms={:.2}",
            frames_ms[frames_ms.len() / 2],
            frames_ms[frames_ms.len() * 95 / 100]
        );
    }

    /// A partial recovery shows its card with keyboard routes: Send waits and keeps the
    /// draft, Shift+Tab from the composer reaches Retry and then Continue, and Continue
    /// returns to the composer without sending.
    #[gpui::test]
    fn a_partial_recovery_holds_send_and_continues_from_the_keyboard(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
        let directory = working_directory();
        let home = dir.path().join("claude-work");
        let folder = home.join("projects/any");
        std::fs::create_dir_all(&folder).unwrap();
        let work = store
            .add_account(Provider::Claude, "Work", home.clone(), &[])
            .unwrap()
            .value;
        store.create(Provider::Claude, &directory, 1).unwrap();
        store.set_account(1, Some(work)).unwrap();
        store
            .set_provider_thread(1, "claude-thread".into())
            .unwrap();
        let cwd = directory.path().to_str().unwrap();
        let records = [
            serde_json::json!({"type": "user", "uuid": "u1", "parentUuid": null,
                "message": {"role": "user", "content": "hello"}}),
            serde_json::json!({"type": "assistant", "uuid": "a1", "parentUuid": "u1",
                "message": {"id": "msg_A", "role": "assistant",
                    "content": [{"type": "text", "text": "Recovered after the crash."}]}}),
        ];
        let mut text = String::new();
        for mut record in records {
            record["sessionId"] = "claude-thread".into();
            record["cwd"] = cwd.into();
            record["isSidechain"] = false.into();
            text.push_str(&format!("{record}\n"));
        }
        std::fs::write(folder.join("claude-thread.jsonl"), text).unwrap();
        let mut log =
            keystone_agent::log::SessionLog::open(&dir.path().join("sessions"), 1).unwrap();
        let prompt = |id: &str| Item {
            id: id.into(),
            kind: ItemKind::UserMessage,
            text: "hello".into(),
            status: ItemStatus::Completed,
        };
        for entry in [
            keystone_agent::log::Entry::Item {
                item: prompt("local:1-0"),
                seq: Some(0),
                run: Some(1),
            },
            keystone_agent::log::Entry::TurnStarted,
            keystone_agent::log::Entry::Item {
                item: prompt("u1"),
                seq: Some(0),
                run: Some(1),
            },
        ] {
            log.append(&entry).unwrap();
        }
        log.sync().unwrap();
        drop((log, store));
        let (view, cx) = open_window(&dir.path().join("workspace.json"), cx);
        view.update(cx, |view, cx| view.select(1, cx));
        wait_until(cx, &view, |view, cx| {
            matches!(
                view.state.read(cx).conversations()[&1].recovery,
                Some(conversation::Recovery::Review(_))
            )
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("recovery-continue").is_some());
        assert!(cx.debug_bounds("recovery-retry").is_some());
        let shown = view.read_with(cx, |view, cx| {
            let rows = &view.state.read(cx).conversations()[&1].rows;
            rows.iter()
                .position(|row| matches!(row, Row::Recovery(_)))
                .unwrap()
        });
        let report = cx
            .debug_bounds(format!("recovery-report-{shown}").leak())
            .unwrap();
        let row = cx
            .debug_bounds(format!("transcript-row-{shown}").leak())
            .unwrap();
        assert!(
            report.right() <= row.right() && report.size.height > px(60.),
            "the report wraps inside its row: {report:?} in {row:?}"
        );
        view.read_with(cx, |view, cx| {
            let rows = &view.state.read(cx).conversations()[&1].rows;
            assert!(rows.iter().any(|row| matches!(row, Row::Recovery(_))));
            assert!(rows.iter().any(|row| matches!(row,
                Row::Item(item) if item.text == "Recovered after the crash.")));
        });

        send_text(&view, cx, "my draft");
        view.read_with(cx, |view, cx| {
            let conversation = &view.state.read(cx).conversations()[&1];
            assert!(conversation.recovery.is_some() && !conversation.running);
            assert_eq!(view.composer.read(cx).value(), "my draft");
        });

        view.update_in(cx, |view, window, cx| view.focus_composer(window, cx));
        cx.simulate_keystrokes("shift-tab");
        cx.update(|window, cx| {
            assert!(
                view.read(cx).recovery_focus.1.is_focused(window),
                "Shift+Tab from the composer reaches Retry"
            )
        });
        cx.simulate_keystrokes("shift-tab");
        cx.update(|window, cx| {
            assert!(
                view.read(cx).recovery_focus.0.is_focused(window),
                "then Continue"
            )
        });
        view.update_in(cx, |view, window, cx| view.focus_composer(window, cx));
        cx.simulate_keystrokes("ctrl-shift-enter");
        cx.run_until_parked();
        assert!(composer_focused(&view, cx));
        view.read_with(cx, |view, cx| {
            let conversation = &view.state.read(cx).conversations()[&1];
            assert!(conversation.recovery.is_none());
            assert!(!conversation.running, "continuing sends nothing");
            assert_eq!(view.composer.read(cx).value(), "my draft");
        });
        assert!(cx.debug_bounds("recovery-continue").is_none());
    }
}
/// The app's keyboard shortcuts, shared by `main` and the GPUI tests.
fn bind_keys(cx: &mut App) {
    TextInput::bind_keys(cx);
    customization::bind_keys(cx);
    cx.bind_keys(commands::bindings());
    files::bind_keys(cx);
    changes::bind_keys(cx);
    cx.bind_keys([
        gpui::KeyBinding::new(
            "tab",
            NextFocus,
            Some("Keystone && !Dialog && !CommandPalette"),
        ),
        gpui::KeyBinding::new(
            "shift-tab",
            PreviousFocus,
            Some("Keystone && !Dialog && !CommandPalette"),
        ),
        // The recovery card sits inside the transcript, whose Tab moves between row
        // actions; within the card Tab moves between its buttons.
        gpui::KeyBinding::new(
            "tab",
            NextFocus,
            Some("Recovery && !Dialog && !CommandPalette"),
        ),
        gpui::KeyBinding::new(
            "shift-tab",
            PreviousFocus,
            Some("Recovery && !Dialog && !CommandPalette"),
        ),
        gpui::KeyBinding::new(
            "ctrl-enter",
            SendMessage,
            Some("!Dialog && !CommandPalette"),
        ),
        gpui::KeyBinding::new("ctrl-k", OpenPalette, Some("!Dialog")),
        gpui::KeyBinding::new("ctrl-shift-p", OpenPalette, Some("!Dialog")),
        // Tab would otherwise type into the palette's single-line query.
        gpui::KeyBinding::new("tab", keystone_ui::text_input::Down, Some("CommandPalette")),
        gpui::KeyBinding::new(
            "shift-tab",
            keystone_ui::text_input::Up,
            Some("CommandPalette"),
        ),
    ]);
}

fn state_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("KEYSTONE_STATE_PATH") {
        return Ok(path.into());
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .ok_or("Set KEYSTONE_STATE_PATH or HOME to select a state location")?;
    Ok(base.join("keystone/workspace.json"))
}

fn main() {
    let path = state_path().unwrap_or_else(|error| {
        eprintln!("Cannot locate Keystone workspace: {error}");
        std::process::exit(1);
    });
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(parent).expect("create Keystone data directory");
    let state_lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_extension("lock"))
        .expect("open workspace lock");
    if let Err(error) = state_lock.try_lock() {
        eprintln!("Cannot lock workspace. Another Keystone process may be using it: {error}");
        std::process::exit(1);
    }
    let store = Store::open(path).map_err(|e| e.to_string());
    let store = match store {
        Ok(store) => store,
        Err(error) => {
            eprintln!("Cannot open Keystone workspace: {error}");
            std::process::exit(1);
        }
    };
    gpui_platform::application()
        .with_quit_mode(gpui::QuitMode::Explicit)
        .with_assets(keystone_ui::Icons)
        .run(move |cx: &mut App| {
            keystone_ui::init(cx);
            bind_keys(cx);
            let state = install_app_state(store, cx);
            let fixture = std::env::var_os("KEYSTONE_VISUAL_FIXTURE").is_some();
            let saved = state.read(cx).store().workspace().preferences().windows();
            let mut opened = 0;
            for (id, placement) in saved {
                match windows::open(state.clone(), id, placement, true, fixture, cx) {
                    Ok(()) => opened += 1,
                    Err(error) => eprintln!("Could not open a Keystone window: {error}"),
                }
            }
            if opened == 0 {
                eprintln!("Cannot open any Keystone window");
                std::process::exit(1);
            }
            cx.activate(true);
        });
}
