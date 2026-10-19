use crate::Keystone;
use crate::arrangement::Arrangement;
use crate::arrangement_editor::{Customization, RequestIds};
use crate::view_state::ViewStates;
use gpui::{App, AppContext, Context, Entity, FocusHandle, Focusable, Window};
use keystone_core::{
    Composition, Preferences, ProjectId, Provider, SessionContext, WorkingDirectory,
};
use keystone_ui::prelude::*;
use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Activation(u64);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProjectPickerTicket {
    request: u64,
    activation: Activation,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LegacyFolderTicket {
    request: u64,
    activation: Activation,
    session: u64,
}

pub(crate) struct ProjectPresentation {
    pub customization: Customization,
    pub arrangement: Arrangement,
    pub views: ViewStates,
    pub customize_prompt: Entity<TextInput>,
    pub customize_provider: Provider,
    pub sidebar: bool,
    pub sidebar_before_customize: Option<bool>,
    pub borrow_focus: Option<FocusHandle>,
    pub folder_focus: FocusHandle,
}
impl ProjectPresentation {
    fn new(initial: &Composition, requests: RequestIds, cx: &mut Context<Keystone>) -> Self {
        Self {
            customization: Customization::new(initial, requests),
            arrangement: Arrangement::default(),
            views: ViewStates::default(),
            customize_prompt: cx
                .new(|cx| TextInput::new("Describe a layout change…", InputMode::SingleLine, cx)),
            customize_provider: Provider::Claude,
            sidebar: true,
            sidebar_before_customize: None,
            borrow_focus: None,
            folder_focus: cx.focus_handle(),
        }
    }
    fn suspend(&mut self) -> Option<u64> {
        self.arrangement.suspend();
        self.views.suspend();
        self.borrow_focus = None;
        self.customization.suspend()
    }
}

pub(crate) struct Presentation {
    pub current: ProjectPresentation,
    pub project: Result<ProjectId, String>,
    parked: HashMap<Option<ProjectId>, ProjectPresentation>,
    activation: Activation,
    next_picker: u64,
    project_picker: Option<ProjectPickerTicket>,
    legacy_picker: Option<LegacyFolderTicket>,
    requests: RequestIds,
    focus_pending: bool,
}
impl Presentation {
    pub fn new(
        project: Result<ProjectId, String>,
        preferences: &Preferences,
        cx: &mut Context<Keystone>,
    ) -> Self {
        let initial = project.as_ref().map_or_else(
            |_| Composition::from_sidebar(preferences.sidebar_layout().shown()),
            |p| preferences.composition(p),
        );
        let requests = RequestIds::default();
        Self {
            current: ProjectPresentation::new(&initial, requests.clone(), cx),
            project,
            parked: HashMap::new(),
            activation: Activation(0),
            next_picker: 0,
            project_picker: None,
            legacy_picker: None,
            requests,
            focus_pending: false,
        }
    }
    pub fn activation(&self) -> Activation {
        self.activation
    }
    pub fn matches(&self, activation: Activation) -> bool {
        self.activation == activation
    }
    pub fn creation_directory(&self) -> Result<WorkingDirectory, String> {
        WorkingDirectory::new(self.project.as_ref().map_err(Clone::clone)?.path())
            .map_err(|e| e.to_string())
    }
    fn request(&mut self) -> Option<u64> {
        self.next_picker = self.next_picker.checked_add(1)?;
        Some(self.next_picker)
    }
    fn project_ticket(&mut self) -> Option<ProjectPickerTicket> {
        let ticket = ProjectPickerTicket {
            request: self.request()?,
            activation: self.activation,
        };
        self.project_picker = Some(ticket);
        Some(ticket)
    }
    fn accept_project_ticket(&mut self, ticket: ProjectPickerTicket) -> bool {
        if self.project_picker != Some(ticket) || !self.matches(ticket.activation) {
            return false;
        }
        self.project_picker = None;
        true
    }
    fn legacy_ticket(&mut self, session: u64) -> Option<LegacyFolderTicket> {
        let ticket = LegacyFolderTicket {
            request: self.request()?,
            activation: self.activation,
            session,
        };
        self.legacy_picker = Some(ticket);
        Some(ticket)
    }
    pub fn selection_changed(&mut self) {
        self.legacy_picker = None;
    }
    pub fn invalidate(
        &mut self,
        session: u64,
        rows: usize,
        changed: &[Range<usize>],
        status: bool,
    ) {
        self.current
            .views
            .invalidate(session, rows, changed, status);
        for p in self.parked.values_mut() {
            p.views.invalidate(session, rows, changed, status);
        }
    }
}

pub(crate) enum FolderPick {
    Cancelled,
    Selected(PathBuf),
    Failed(String),
}
impl FolderPick {
    pub fn from_native(
        result: Result<anyhow::Result<Option<Vec<PathBuf>>>, futures::channel::oneshot::Canceled>,
    ) -> Self {
        match result {
            Ok(Ok(Some(mut paths))) if paths.len() == 1 => Self::Selected(paths.remove(0)),
            Ok(Err(error)) => Self::Failed(format!("Couldn't open a folder picker: {error}")),
            Err(error) => Self::Failed(format!("Couldn't open a folder picker: {error}")),
            _ => Self::Cancelled,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConversationFolder {
    Recorded(WorkingDirectory),
    LegacyMissing,
    Unsupported,
}
impl ConversationFolder {
    pub fn tooltip(&self) -> String {
        match self {
            Self::Recorded(directory) => format!(
                "Open conversation folder · {} · {}",
                directory
                    .path()
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy(),
                directory.as_str()
            ),
            Self::LegacyMissing => "Choose conversation folder".into(),
            Self::Unsupported => {
                "Saved conversation folder is not supported by this version".into()
            }
        }
    }
    pub fn button(&self, id: &'static str) -> IconButton {
        IconButton::new(id, IconName::Folder)
            .size(ButtonSize::Default)
            .disabled(matches!(self, Self::Unsupported))
    }
}
impl Keystone {
    pub(crate) fn scoped_listener<E: ?Sized + 'static, F>(
        &self,
        cx: &Context<Self>,
        listener: F,
    ) -> impl Fn(&E, &mut Window, &mut App) + 'static + use<E, F>
    where
        F: Fn(&mut Self, &E, &mut Window, &mut Context<Self>) + 'static,
    {
        let activation = self.presentation.activation();
        let entity = cx.weak_entity();
        move |event, window, cx| {
            let _ = entity.update(cx, |this, cx| {
                if this.presentation.matches(activation) {
                    listener(this, event, window, cx);
                }
            });
        }
    }
    pub(crate) fn conversation_folder(&self, session: u64) -> Option<ConversationFolder> {
        self.store
            .workspace()
            .sessions()
            .iter()
            .find(|s| s.id() == session)
            .map(|s| match s.context() {
                SessionContext::Recorded { directory, .. } => {
                    ConversationFolder::Recorded(directory.clone())
                }
                SessionContext::LegacyMissing => ConversationFolder::LegacyMissing,
                SessionContext::Unsupported => ConversationFolder::Unsupported,
            })
    }
    pub(crate) fn project_tooltip(&self) -> String {
        match &self.presentation.project {
            Ok(project) => format!(
                "New project · Current layout and new sessions · {}",
                project.path().display()
            ),
            Err(error) => format!("New project · Current project unavailable · {error}"),
        }
    }
    pub(crate) fn choose_project(&mut self, cx: &mut Context<Self>) {
        let Some(ticket) = self.presentation.project_ticket() else {
            self.error = Some("No more folder requests are available.".into());
            cx.notify();
            return;
        };
        let picked = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open project".into()),
        });
        cx.spawn(async move |this, cx| {
            let outcome = FolderPick::from_native(picked.await);
            let _ = this.update(cx, |this, cx| this.finish_project_pick(ticket, outcome, cx));
        })
        .detach();
    }
    fn finish_project_pick(
        &mut self,
        ticket: ProjectPickerTicket,
        outcome: FolderPick,
        cx: &mut Context<Self>,
    ) {
        if !self.presentation.accept_project_ticket(ticket) {
            return;
        }
        let path = match outcome {
            FolderPick::Cancelled => return,
            FolderPick::Failed(error) => {
                self.error = Some(error);
                self.refresh(cx);
                return;
            }
            FolderPick::Selected(path) => path,
        };
        let project = match ProjectId::new(&path) {
            Ok(project) => project,
            Err(error) => {
                self.error = Some(format!("Project not opened. {error}"));
                self.refresh(cx);
                return;
            }
        };
        if self.presentation.project.as_ref().ok() == Some(&project) {
            return;
        }
        let Some(next) = self
            .presentation
            .activation
            .0
            .checked_add(1)
            .map(Activation)
        else {
            self.error = Some("No more project activations are available.".into());
            self.refresh(cx);
            return;
        };
        let prepared = if self
            .presentation
            .parked
            .contains_key(&Some(project.clone()))
        {
            None
        } else {
            Some(ProjectPresentation::new(
                &self.store.workspace().preferences().composition(&project),
                self.presentation.requests.clone(),
                cx,
            ))
        };
        if !self.save_draft(cx) {
            return;
        }
        let outgoing = self.presentation.current.suspend();
        if let Some(request) = outgoing
            && let Some(agent) = &self.customize_agent
        {
            agent.cancel(request);
        }
        let destination = prepared.unwrap_or_else(|| {
            self.presentation
                .parked
                .remove(&Some(project.clone()))
                .unwrap()
        });
        let old = std::mem::replace(&mut self.presentation.current, destination);
        self.presentation
            .parked
            .insert(self.presentation.project.as_ref().ok().cloned(), old);
        self.presentation.project = Ok(project);
        self.presentation.activation = next;
        self.presentation.selection_changed();
        self.presentation.focus_pending = true;
        self.new_menu = false;
        self.filter_menu = false;
        self.settings = None;
        self.approval_menu = false;
        self.confirm_full_access = None;
        self.window_drag = false;
        self.refresh(cx);
    }
    pub(crate) fn focus_project_transition(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !std::mem::take(&mut self.presentation.focus_pending) {
            return;
        }
        if self.renaming.is_some() {
            self.rename_input.focus_handle(cx).focus(window, cx);
        } else if self.presentation.current.customization.is_open() {
            self.presentation
                .current
                .customize_prompt
                .focus_handle(cx)
                .focus(window, cx);
        } else {
            self.focus.focus(window, cx);
        }
    }
    pub(crate) fn choose_legacy_folder(&mut self, session: u64, cx: &mut Context<Self>) {
        if self.store.workspace().selected() != Some(session)
            || self.conversation_folder(session) != Some(ConversationFolder::LegacyMissing)
        {
            return;
        }
        let Some(ticket) = self.presentation.legacy_ticket(session) else {
            return;
        };
        let picked = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose conversation folder".into()),
        });
        cx.spawn(async move |this, cx| {
            let outcome = FolderPick::from_native(picked.await);
            let _ = this.update(cx, |this, cx| {
                this.finish_legacy_folder(ticket, outcome, cx)
            });
        })
        .detach();
    }
    fn finish_legacy_folder(
        &mut self,
        ticket: LegacyFolderTicket,
        outcome: FolderPick,
        cx: &mut Context<Self>,
    ) {
        if self.presentation.legacy_picker != Some(ticket)
            || !self.presentation.matches(ticket.activation)
            || self.store.workspace().selected() != Some(ticket.session)
            || self.conversation_folder(ticket.session) != Some(ConversationFolder::LegacyMissing)
        {
            return;
        }
        self.presentation.legacy_picker = None;
        let path = match outcome {
            FolderPick::Cancelled => return,
            FolderPick::Failed(error) => {
                self.folder_error(ticket.session, error, cx);
                return;
            }
            FolderPick::Selected(path) => path,
        };
        let directory = match WorkingDirectory::new(&path) {
            Ok(directory) => directory,
            Err(_) => {
                self.folder_error(ticket.session, "Not sent. That folder is unavailable. Choose an existing folder with the folder button. Your draft is kept.".into(), cx);
                return;
            }
        };
        match self
            .store
            .choose_legacy_directory(ticket.session, &directory)
        {
            Ok(applied) => {
                self.conversation(ticket.session).error = None;
                self.folder_warning =
                    (!self.store.is_durable(applied.revision)).then_some(crate::FolderWarning {
                        session: ticket.session,
                        revision: applied.revision,
                    });
                self.commit(ticket.session);
                self.refresh(cx);
            }
            Err(error) => self.folder_error(
                ticket.session,
                format!(
                    "Not sent. The folder choice could not be saved. {error}. Your draft is kept."
                ),
                cx,
            ),
        }
    }
    fn folder_error(&mut self, session: u64, error: String, cx: &mut Context<Self>) {
        self.conversation(session).error = Some(error);
        self.commit(session);
        self.refresh(cx);
    }
    pub(crate) fn dispatch_directory(
        &mut self,
        session: u64,
        cx: &mut Context<Self>,
    ) -> Option<WorkingDirectory> {
        let result = match self.conversation_folder(session)? {
            ConversationFolder::Recorded(directory) => match std::fs::metadata(directory.path()) {
                Ok(metadata)
                    if metadata.is_dir()
                        && std::ffi::CString::new(directory.as_str()).is_ok_and(
                            |path| unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 },
                        ) =>
                {
                    Ok(directory)
                }
                _ => Err(
                    "Not sent. This conversation's saved folder is unavailable. Your draft is kept.",
                ),
            },
            ConversationFolder::LegacyMissing => Err(
                "Not sent. Choose a conversation folder using the folder button beside the model. Your draft is kept.",
            ),
            ConversationFolder::Unsupported => Err(
                "Not sent. This conversation has saved folder data that this version cannot use. Your draft is kept.",
            ),
        };
        match result {
            Ok(directory) => Some(directory),
            Err(error) => {
                self.folder_error(session, error.into(), cx);
                None
            }
        }
    }
}

#[cfg(all(test, feature = "ui-tests"))]
#[path = "presentation_tests.rs"]
mod tests;
