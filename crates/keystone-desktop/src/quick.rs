//! The quick composer: a small window that starts a session and sends its first message
//! in one step. `keystone --quick` opens it in the running app over a Unix socket, so a
//! compositor binding or a hotkey tool can call it, and starts the app first when none
//! runs. Escape closes it and keeps the text for the next opening. A launch without
//! `--quick` asks the running app over the same socket to show its windows.

use crate::app_state::Phase;
use crate::model_selector::{self, CatalogKey, Choices, Selection};
use crate::{AppState, ComposerDraft, DesktopApp};
use futures::StreamExt;
use futures::channel::mpsc::UnboundedSender;
use futures::channel::oneshot;
use gpui::{
    Anchor, App, AppContext, Bounds, Context, Entity, Focusable, InteractiveElement, IntoElement,
    ParentElement, Render, StatefulInteractiveElement, Styled, Task, Window, WindowBounds,
    WindowOptions, div, prelude::FluentBuilder, px, size,
};
use keystone_core::{AccountId, ProjectId, Provider, QuickDraft, WorkingDirectory};
use keystone_ui::automation;
use keystone_ui::prelude::*;
use keystone_ui::text_input::{Changed, InputMode, TextInput};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

gpui::actions!(keystone, [OpenQuickComposer, SendQuick, DismissQuick]);

pub(crate) const FLAG: &str = "--quick";
const TITLE: &str = "Quick composer";
const ANSWER: &str = "ok\n";
/// The IDs catalogs answer the quick composer under, one per request so answers for
/// two folders never cross. Session IDs count up from 1 and never reach them.
static ASKERS: AtomicU64 = AtomicU64::new(u64::MAX);
const SIZE: (f32, f32) = (640., 320.);
const NO_PROJECT: &str = "Choose a project.";

/// Where the app holding the workspace at `state` listens: the runtime folder where
/// there is one, as on Linux, else the workspace's own folder, as on macOS, or the
/// temporary folder when that path is too long for a socket. It is named after the
/// workspace database, so each workspace's app has its own.
pub(crate) fn socket(state: &Path) -> PathBuf {
    let database = keystone_core::database_path(state);
    let database = match (
        database.parent().map(Path::canonicalize),
        database.file_name(),
    ) {
        (Some(Ok(folder)), Some(name)) => folder.join(name),
        _ => std::path::absolute(&database).unwrap_or(database),
    };
    let folder = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|folder| !folder.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| database.parent().map(Path::to_owned).unwrap_or_default());
    let digest = Sha256::digest(database.as_os_str().as_encoded_bytes());
    let name: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    let name = format!("keystone-{name}.sock");
    let socket = folder.join(&name);
    if socket.as_os_str().len() < 104 {
        return socket;
    }
    std::env::temp_dir().join(name)
}

/// What a launch asks the app already holding the workspace for.
#[derive(Clone, Copy)]
pub(crate) enum Request {
    /// The quick composer, for `--quick`.
    Quick,
    /// Its windows, opened again when none is open.
    Windows,
}

impl Request {
    fn line(self) -> &'static str {
        match self {
            Self::Quick => "quick\n",
            Self::Windows => "windows\n",
        }
    }
}

/// Asks the app listening on `socket` for `request`. False when none answers within
/// `wait`.
pub(crate) fn ask(socket: &Path, request: Request, wait: Duration) -> bool {
    let Ok(mut stream) = UnixStream::connect(socket) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(wait.max(Duration::from_millis(100))));
    let mut answer = String::new();
    stream.write_all(request.line().as_bytes()).is_ok()
        && BufReader::new(stream).read_line(&mut answer).is_ok()
        && answer == ANSWER
}

/// Answers each request on `socket` while the app runs. Only the process holding the
/// workspace lock listens, so a socket already there is stale.
pub(crate) fn listen(socket: &Path, cx: &mut App) -> std::io::Result<()> {
    match std::fs::remove_file(socket) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let listener = UnixListener::bind(socket)?;
    let (requests, mut received) = futures::channel::mpsc::unbounded();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => answer(stream, &requests),
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    });
    cx.spawn(async move |cx| {
        while let Some((request, shown)) = received.next().await {
            let _ = shown.send(cx.update(|cx| match request {
                Request::Quick => open(cx),
                Request::Windows => crate::windows::show_all(cx),
            }));
        }
    })
    .detach();
    let socket = socket.to_owned();
    cx.on_app_quit(move |_| {
        let _ = std::fs::remove_file(&socket);
        async {}
    })
    .detach();
    Ok(())
}

/// Answers once what was asked for shows, so the launch that asked fails when it
/// cannot.
fn answer(stream: UnixStream, requests: &UnboundedSender<(Request, oneshot::Sender<bool>)>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let mut line = String::new();
    if BufReader::new(&stream).read_line(&mut line).is_err() {
        return;
    }
    let Some(request) = [Request::Quick, Request::Windows]
        .into_iter()
        .find(|request| request.line() == line)
    else {
        return;
    };
    let (opened, shown) = oneshot::channel();
    if requests.unbounded_send((request, opened)).is_ok()
        && futures::executor::block_on(shown) == Ok(true)
    {
        let _ = (&stream).write_all(ANSWER.as_bytes());
    }
}

/// Opens the quick composer, or brings it forward when it is open. While the last
/// window's turn runs on with no window open, the windows open again too, so Keystone
/// stays open rather than quitting once idle. Says whether it shows.
pub(crate) fn open(cx: &mut App) -> bool {
    let Some(app) = cx.try_global::<DesktopApp>() else {
        return false;
    };
    let (state, quick) = (app.state.clone(), app.quick);
    if state.read(cx).phase != Phase::Running {
        return false;
    }
    cx.activate(true);
    crate::windows::reopen(state.clone(), cx);
    if let Some(window) = quick
        && window
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        return true;
    }
    let bounds = Bounds::centered(None, size(px(SIZE.0), px(SIZE.1)), cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: None,
            app_id: Some("keystone".into()),
            ..Default::default()
        },
        |window, cx| {
            let view = cx.new(|cx| QuickComposer::new(state, window, cx));
            let closing = view.downgrade();
            window.on_window_should_close(cx, move |window, cx| {
                let kept = closing
                    .update(cx, |view, cx| view.keep(cx).is_ok())
                    .unwrap_or(true);
                if kept {
                    automation::forget_window(window.window_handle().window_id().as_u64());
                }
                kept
            });
            view
        },
    );
    match opened {
        Ok(window) => {
            cx.global_mut::<DesktopApp>().quick = Some(window);
            true
        }
        Err(error) => {
            eprintln!("Could not open the quick composer: {error}");
            false
        }
    }
}

/// Saves the open quick composer's text and choices, as Quit does for every draft.
pub(crate) fn keep_open(cx: &mut App) -> Result<(), String> {
    let Some(window) = cx.try_global::<DesktopApp>().and_then(|app| app.quick) else {
        return Ok(());
    };
    window
        .update(cx, |view, _, cx| view.keep(cx))
        .unwrap_or(Ok(()))
}

pub(crate) struct QuickComposer {
    state: Entity<AppState>,
    input: Entity<TextInput>,
    project: Option<ProjectId>,
    provider: Provider,
    account: Option<AccountId>,
    model: Option<String>,
    model_menu: bool,
    project_menu: bool,
    error: Option<String>,
    save_task: Option<Task<()>>,
    _subscriptions: [gpui::Subscription; 2],
}

impl QuickComposer {
    fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let workspace = state.read(cx).store().workspace();
        let saved = workspace.preferences().quick().clone();
        let projects = workspace.projects();
        let project = saved
            .project
            .as_deref()
            .and_then(|path| projects.iter().find(|p| p.id().path() == Path::new(path)))
            .or(projects.first())
            .map(|p| p.id());
        let provider = saved.provider.unwrap_or(Provider::Claude);
        let account = saved.account.clone().filter(|id| {
            workspace
                .account(id)
                .is_some_and(|a| a.provider() == provider)
        });
        let model = saved.model.filter(|_| account == saved.account);
        let input = cx.new(|cx| {
            let mut input = TextInput::new("Write a draft…", InputMode::Multiline, cx)
                .text_style(TextSize::Body, Color::Strong)
                .fill_height();
            input.set_value(saved.text, cx);
            input
        });
        input.focus_handle(cx).focus(window, cx);
        window.set_window_title(TITLE);
        let subscriptions = [
            cx.subscribe(&input, |this, _, _: &Changed, cx| {
                this.keep_soon(cx);
                cx.notify();
            }),
            cx.observe(&state, |_, _, cx| cx.notify()),
        ];
        let mut this = Self {
            state,
            input,
            project,
            provider,
            account,
            model,
            model_menu: false,
            project_menu: false,
            error: None,
            save_task: None,
            _subscriptions: subscriptions,
        };
        this.request_catalog(cx);
        this
    }

    fn draft(&self, cx: &App) -> QuickDraft {
        QuickDraft {
            text: self.input.read(cx).value().to_owned(),
            project: self
                .project
                .as_ref()
                .map(|p| p.path().to_string_lossy().into_owned()),
            provider: Some(self.provider),
            account: self.account.clone(),
            model: self.model.clone(),
        }
    }

    /// Saves the draft, showing why when it cannot.
    fn keep(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        self.save_task = None;
        let draft = self.draft(cx);
        let saved = self
            .state
            .update(cx, |state, _| state.set_quick_draft(draft))
            .map(|_| ())
            .map_err(|error| error.to_string());
        if let Err(error) = &saved {
            self.error = Some(format!("Draft not saved. {error}"));
            cx.notify();
        }
        saved
    }

    fn keep_soon(&mut self, cx: &mut Context<Self>) {
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
            let _ = this.update(cx, |this, cx| this.keep(cx));
        }));
    }

    fn catalog_key(&self) -> Option<CatalogKey> {
        Some(CatalogKey {
            provider: self.provider,
            account: self.account.clone(),
            directory: WorkingDirectory::new(self.project.as_ref()?.path()).ok()?,
        })
    }

    fn request_catalog(&mut self, cx: &mut Context<Self>) {
        if let Some(key) = self.catalog_key() {
            let asker = ASKERS.fetch_sub(1, Ordering::Relaxed);
            self.state
                .update(cx, |state, cx| state.request_catalog_for(key, asker, cx));
        }
    }

    fn chose(&mut self, cx: &mut Context<Self>) {
        self.request_catalog(cx);
        self.keep_soon(cx);
        cx.notify();
    }

    /// Closes once the draft is saved; a failed save stays open with its error.
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.keep(cx).is_ok() {
            automation::forget_window(window.window_handle().window_id().as_u64());
            window.remove_window();
        }
    }

    fn dismiss(&mut self, _: &DismissQuick, window: &mut Window, cx: &mut Context<Self>) {
        if self.model_menu || self.project_menu {
            self.model_menu = false;
            self.project_menu = false;
            return cx.notify();
        }
        self.close(window, cx);
    }

    /// Creates the session with the chosen project, provider, account and model, sends
    /// the text and closes. A send the session refuses leaves the text as its draft and
    /// shows it in a Keystone window, with the reason.
    fn send(&mut self, _: &SendQuick, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_owned();
        if text.is_empty() {
            return;
        }
        let session = match self.create(cx) {
            Ok(session) => session,
            Err(error) => {
                self.error = Some(format!("Not sent. {error}"));
                return cx.notify();
            }
        };
        let draft = ComposerDraft {
            session,
            text: text.clone(),
            attachments: Vec::new(),
        };
        let sent = self
            .state
            .update(cx, |state, cx| state.send_message(draft, cx));
        if !sent
            && let Err(error) = self
                .state
                .update(cx, |state, cx| state.set_draft(session, text, cx))
        {
            self.error = Some(format!("Draft not saved. {error}"));
            return cx.notify();
        }
        self.error = None;
        self.input
            .update(cx, |input, cx| input.set_value(String::new(), cx));
        self.close(window, cx);
        if !sent {
            cx.defer(move |cx| crate::notifications::show(session, cx));
        }
    }

    fn create(&mut self, cx: &mut Context<Self>) -> Result<u64, String> {
        let project = self.project.as_ref().ok_or(NO_PROJECT)?;
        let directory = WorkingDirectory::new(project.path()).map_err(|e| e.to_string())?;
        let (provider, account, model) = (self.provider, self.account.clone(), self.model.clone());
        self.state
            .update(cx, |state, cx| {
                state.create_quick(provider, account, model, &directory, cx)
            })
            .map(|applied| applied.value)
            .map_err(|e| e.to_string())
    }

    fn choose_provider(&mut self, provider: Provider, cx: &mut Context<Self>) {
        if provider == self.provider {
            self.model_menu = false;
            return cx.notify();
        }
        (self.provider, self.account, self.model) = (provider, None, None);
        self.chose(cx);
    }

    fn choose_account(
        &mut self,
        provider: Provider,
        account: Option<AccountId>,
        cx: &mut Context<Self>,
    ) {
        if (provider, &account) == (self.provider, &self.account) {
            self.model_menu = false;
            return cx.notify();
        }
        (self.provider, self.account, self.model) = (provider, account, None);
        self.chose(cx);
    }

    fn choose_model(&mut self, provider: Provider, model: Option<String>, cx: &mut Context<Self>) {
        self.model_menu = false;
        if provider != self.provider || model == self.model {
            return cx.notify();
        }
        self.model = model;
        self.chose(cx);
    }

    fn choose_project(&mut self, project: ProjectId, cx: &mut Context<Self>) {
        self.project_menu = false;
        if self.project.as_ref() == Some(&project) {
            return cx.notify();
        }
        self.project = Some(project);
        self.error = None;
        self.chose(cx);
    }

    fn model_selector(&self, window: &Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let state = self.state.read(cx);
        let key = self.catalog_key();
        let choices = match &key {
            Some(key) => state.catalog_choices(key),
            None => Choices::Unavailable(NO_PROJECT),
        };
        let selection = Selection {
            provider: self.provider,
            account: self.account.as_ref(),
            accounts: state.store().workspace().accounts(),
            model: self.model.as_deref(),
            reported_model: None,
            started: false,
            choices,
        };
        let handlers = model_selector::Handlers {
            open_change: Rc::new(cx.listener(|this, open: &bool, _, cx| {
                this.model_menu = *open;
                if *open {
                    this.request_catalog(cx);
                }
                cx.notify();
            })),
            choose_provider: Rc::new(
                cx.listener(|this, provider: &Provider, _, cx| this.choose_provider(*provider, cx)),
            ),
            choose_account: Rc::new(cx.listener(
                |this, (provider, account): &(Provider, Option<AccountId>), _, cx| {
                    this.choose_account(*provider, account.clone(), cx)
                },
            )),
            choose_model: Rc::new(cx.listener(
                |this, (provider, model): &(Provider, Option<String>), _, cx| {
                    this.choose_model(*provider, model.clone(), cx)
                },
            )),
            retry: Rc::new(cx.listener(|this, _: &Provider, _, cx| {
                if let Some(key) = this.catalog_key() {
                    let asker = ASKERS.fetch_sub(1, Ordering::Relaxed);
                    this.state
                        .update(cx, |state, cx| state.retry_catalog_for(key, asker, cx));
                }
            })),
        };
        model_selector::render(
            selection,
            self.model_menu,
            handlers,
            window.viewport_size(),
            cx,
        )
        .into_any_element()
    }

    fn project_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let projects: Vec<ProjectId> = self
            .state
            .read(cx)
            .store()
            .workspace()
            .projects()
            .into_iter()
            .map(|p| p.id())
            .collect();
        let name = self.project.as_ref().map_or("", |p| p.name()).to_owned();
        let trigger = Button::new("quick-project", name)
            .style(ButtonStyle::Ghost)
            .icon(IconName::Folder)
            .selected(self.project_menu)
            .debug_selector(|| "quick-project".into());
        PopoverMenu::new("quick-project-menu", trigger)
            .anchor(Anchor::BottomLeft)
            .open(self.project_menu)
            .on_open_change(cx.listener(|this, open: &bool, _, cx| {
                this.project_menu = *open;
                cx.notify();
            }))
            .children(projects.into_iter().enumerate().map(|(index, project)| {
                let checked = self.project.as_ref() == Some(&project);
                MenuItem::new(("quick-project-item", index), project.name().to_owned())
                    .checked(checked)
                    .debug_selector(move || format!("quick-project-{index}"))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.choose_project(project.clone(), cx)),
                    )
            }))
    }
}

impl Render for QuickComposer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors;
        let can_send = self.project.is_some() && !self.input.read(cx).value().trim().is_empty();
        let send = crate::send_button(
            "quick-send",
            can_send,
            Tooltip::with_key("Send", "Enter"),
            cx,
        )
        .when(can_send, |d| {
            d.on_click(cx.listener(|this, _, window, cx| this.send(&SendQuick, window, cx)))
        });
        let toolbar = h_flex()
            .gap(px(14.))
            .child(
                div()
                    .flex_shrink(1.)
                    .min_w(model_selector::MIN_WIDTH)
                    .child(self.model_selector(window, cx)),
            )
            .child(self.project_menu(cx))
            .child(div().flex_1())
            .child(send);
        v_flex()
            .id("quick-composer")
            .key_context("QuickComposer")
            .on_action(cx.listener(Self::send))
            .on_action(cx.listener(Self::dismiss))
            .size_full()
            .bg(colors.composer)
            .font_ui(cx)
            .text_color(colors.text)
            .text_size(TextSize::Base.px())
            .line_height(TextSize::Base.line_height())
            .pt(px(16.))
            .pl(px(18.))
            .pr(px(12.))
            .pb(px(12.))
            .gap(px(14.))
            .child(
                div()
                    .id("quick-field")
                    .automation_id("quick-field")
                    .flex_1()
                    .min_h_0()
                    .pr(px(6.))
                    .child(self.input.clone()),
            )
            .children(
                self.error
                    .clone()
                    .map(|error| Label::new(error).size(TextSize::Sm).color(Color::Danger)),
            )
            .child(toolbar)
            .when(automation::enabled(), |d| {
                d.child(automation::titled_frame_writer(TITLE))
            })
    }
}

#[cfg(all(test, feature = "ui-tests"))]
#[path = "quick_tests.rs"]
mod tests;
