use crate::customization_agent::{Ask, Outcome, Proposal};
use keystone_core::{Component, Composition, Edit, Node, ProjectId, SavedComposition, ViewId};
use std::collections::BTreeMap;

/// The width a Files view opens at beside a conversation.
pub const FILES_WIDTH: keystone_core::Size =
    keystone_core::Size::Px(std::num::NonZeroU16::new(300).unwrap());

#[derive(Clone, Debug, PartialEq)]
pub enum Interpretation {
    Idle,
    Running { request: u64, revision: u64 },
    Failed(String),
    Unsupported(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Chooser {
    Layouts { selected: Option<ProjectId> },
    Replace(ProjectId),
    Unavailable,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Open,
    Edit(Edit),
    Select(ViewId),
    Duplicate(ViewId),
    AddSessionList,
    AddFiles,
    AddChanges,
    Reset,
    Ask(String),
    Retry,
    Cancel,
    Apply,
    Undo,
    Discard,
    Close,
    Borrow,
    Choose(ProjectId),
    PreviewBorrow,
    ReplaceBorrow,
    CloseChooser,
    BackToLayouts,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Ask(Ask),
    Cancel(u64),
    Save(Option<SavedComposition>),
    Borrow(ProjectId),
}

struct Editing {
    candidate: Composition,
    revision: u64,
    interpretation: Interpretation,
    note: Option<String>,
    selected: Option<ViewId>,
    chooser: Option<Chooser>,
}

#[derive(Clone, Default)]
pub struct RequestIds(std::rc::Rc<std::cell::Cell<u64>>);
impl RequestIds {
    fn allocate(&self) -> Option<u64> {
        let next = self.0.get().checked_add(1)?;
        self.0.set(next);
        Some(next)
    }
}

pub struct Customization {
    editing: Option<Editing>,
    replaced: Option<Option<SavedComposition>>,
    last_ask: Option<String>,
    requests: RequestIds,
    next_view: u64,
}

impl Customization {
    pub fn new(initial: &Composition, requests: RequestIds) -> Self {
        Self {
            editing: None,
            replaced: None,
            last_ask: None,
            requests,
            next_view: initial
                .views()
                .map(|(id, _)| u64::from(id.0) + 1)
                .max()
                .unwrap_or(0),
        }
    }

    pub fn suspend(&mut self) -> Option<u64> {
        let running = self.running();
        if let Some(e) = &mut self.editing {
            e.chooser = None;
            if running.is_some() {
                e.interpretation = Interpretation::Failed("Request cancelled.".into());
            }
        }
        running
    }

    pub fn is_open(&self) -> bool {
        self.editing.is_some()
    }
    pub fn shown<'a>(&'a self, saved: &'a Composition) -> &'a Composition {
        self.editing.as_ref().map_or(saved, |e| &e.candidate)
    }
    pub fn selected(&self) -> Option<ViewId> {
        self.editing.as_ref()?.selected
    }
    pub fn chooser(&self) -> Option<&Chooser> {
        self.editing.as_ref()?.chooser.as_ref()
    }
    pub fn interpretation(&self) -> Option<&Interpretation> {
        Some(&self.editing.as_ref()?.interpretation)
    }
    pub fn note(&self) -> Option<&str> {
        self.editing.as_ref()?.note.as_deref()
    }
    pub fn running(&self) -> Option<u64> {
        match self.interpretation()? {
            Interpretation::Running { request, .. } => Some(*request),
            _ => None,
        }
    }
    pub fn can_retry(&self) -> bool {
        self.last_ask.is_some()
            && matches!(
                self.interpretation(),
                Some(Interpretation::Failed(_) | Interpretation::Unsupported(_))
            )
    }
    pub fn can_apply(&self, raw: Option<&SavedComposition>, saved: &Composition) -> bool {
        self.editing
            .as_ref()
            .is_some_and(|e| e.candidate != *saved || raw.is_some_and(|r| r.supported().is_none()))
    }
    pub fn can_discard(&self, saved: &Composition) -> bool {
        self.running().is_some() || self.editing.as_ref().is_some_and(|e| e.candidate != *saved)
    }
    pub fn can_undo(&self, saved: &Composition) -> bool {
        self.replaced.is_some() && !self.can_discard(saved)
    }
    /// Withdraws Undo once the saved layout changed outside Customize, so it cannot
    /// restore a layout from before edits the user made since.
    pub fn forget_undo(&mut self) {
        self.replaced = None;
    }

    /// A view id no composition of this project has used, for an edit outside Customize too.
    pub fn allocate(&mut self) -> Result<ViewId, String> {
        let id = u32::try_from(self.next_view)
            .map_err(|_| "No more component IDs are available.".to_owned())?;
        self.next_view += 1;
        Ok(ViewId(id))
    }

    fn remap(
        &mut self,
        source: &Composition,
        retain: Option<&Composition>,
    ) -> Result<Composition, String> {
        let mut ids = BTreeMap::new();
        for (id, component) in source.views() {
            let next = if retain.is_some_and(|c| c.component(id) == Some(component)) {
                id
            } else {
                self.allocate()?
            };
            ids.insert(id, next);
        }
        fn node(n: &Node, ids: &BTreeMap<ViewId, ViewId>) -> Node {
            match n {
                Node::View { id } => Node::View { id: ids[id] },
                Node::Tabs { selected, views } => Node::Tabs {
                    selected: *selected,
                    views: views.iter().map(|id| ids[id]).collect(),
                },
                Node::Split { axis, children } => Node::Split {
                    axis: *axis,
                    children: children
                        .iter()
                        .map(|c| keystone_core::Child {
                            node: node(&c.node, ids),
                            size: c.size,
                        })
                        .collect(),
                },
            }
        }
        let document = serde_json::json!({
            "version": 1,
            "views": source.views().map(|(id, component)| serde_json::json!({"id": ids[&id], "component": component})).collect::<Vec<_>>(),
            "root": node(source.root(), &ids),
        });
        serde_json::from_value(document).map_err(|e| e.to_string())
    }

    fn cancel(&mut self, effects: &mut Vec<Effect>) {
        if let Some(request) = self.running() {
            effects.push(Effect::Cancel(request));
        }
        if let Some(e) = &mut self.editing {
            e.interpretation = Interpretation::Idle;
        }
    }

    fn stage(&mut self, candidate: Composition, note: Option<String>) {
        let e = self.editing.as_mut().unwrap();
        if e.candidate != candidate {
            e.revision += 1;
        }
        e.candidate = candidate;
        e.selected = e.selected.filter(|id| e.candidate.component(*id).is_some());
        e.note = note;
    }

    pub fn dispatch(
        &mut self,
        action: Action,
        raw: Option<&SavedComposition>,
        saved: &Composition,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        if action == Action::Open {
            self.editing.get_or_insert_with(|| Editing {
                candidate: saved.clone(),
                revision: 0,
                interpretation: Interpretation::Idle,
                selected: saved
                    .views()
                    .find(|(_, c)| *c == Component::Conversation)
                    .map(|(id, _)| id),
                chooser: None,
                note: raw
                    .filter(|r| r.supported().is_none())
                    .map(|_| "Saved layout unreadable. Apply replaces it.".into()),
            });
            return effects;
        }
        if self.editing.is_none() {
            return effects;
        }
        match action {
            Action::Select(id) => {
                self.editing.as_mut().unwrap().selected = Some(id);
                return effects;
            }
            Action::Borrow | Action::BackToLayouts => {
                self.editing.as_mut().unwrap().chooser = Some(Chooser::Layouts { selected: None });
                return effects;
            }
            Action::Choose(id) => {
                self.editing.as_mut().unwrap().chooser =
                    Some(Chooser::Layouts { selected: Some(id) });
                return effects;
            }
            Action::CloseChooser => {
                self.editing.as_mut().unwrap().chooser = None;
                return effects;
            }
            Action::PreviewBorrow => {
                if let Some(Chooser::Layouts { selected: Some(id) }) = self.chooser().cloned() {
                    if self.can_discard(saved) {
                        self.editing.as_mut().unwrap().chooser = Some(Chooser::Replace(id));
                    } else {
                        effects.push(Effect::Borrow(id));
                    }
                }
                return effects;
            }
            Action::ReplaceBorrow => {
                if let Some(Chooser::Replace(id)) = self.chooser().cloned() {
                    effects.push(Effect::Borrow(id));
                }
                return effects;
            }
            Action::Undo if !self.can_undo(saved) => return effects,
            _ => {}
        }
        self.cancel(&mut effects);
        let current = self.editing.as_ref().unwrap().candidate.clone();
        let candidate = match action {
            Action::Edit(edit) => current.edit(edit).map_err(|e| e.to_string()),
            Action::Duplicate(target) => self.allocate().and_then(|view| {
                current
                    .edit(Edit::Open {
                        view,
                        component: Component::Conversation,
                        at: keystone_core::Place::Beside {
                            target,
                            axis: keystone_core::Axis::Column,
                            after: true,
                        },
                        size: keystone_core::Size::Fill(std::num::NonZeroU16::MIN),
                    })
                    .map_err(|e| e.to_string())
            }),
            Action::AddSessionList => self.allocate().and_then(|view| {
                current
                    .edit(Edit::Open {
                        view,
                        component: Component::SessionList,
                        at: keystone_core::Place::Beside {
                            target: current
                                .views()
                                .find(|(_, c)| *c == Component::Conversation)
                                .unwrap()
                                .0,
                            axis: keystone_core::Axis::Row,
                            after: false,
                        },
                        size: keystone_core::Size::Px(
                            std::num::NonZeroU16::new(keystone_core::SidebarWidth::DEFAULT.px())
                                .unwrap(),
                        ),
                    })
                    .map_err(|e| e.to_string())
            }),
            add @ (Action::AddFiles | Action::AddChanges) => self.allocate().and_then(|view| {
                current
                    .edit(Edit::Open {
                        view,
                        component: if add == Action::AddFiles {
                            Component::Files
                        } else {
                            Component::Changes
                        },
                        at: keystone_core::Place::Beside {
                            target: current
                                .views()
                                .find(|(_, c)| *c == Component::Conversation)
                                .unwrap()
                                .0,
                            axis: keystone_core::Axis::Row,
                            after: true,
                        },
                        size: FILES_WIDTH,
                    })
                    .map_err(|e| e.to_string())
            }),
            Action::Reset => self.remap(&Composition::default(), Some(&current)),
            Action::Discard => Ok(saved.clone()),
            Action::Close => {
                self.editing = None;
                return effects;
            }
            Action::Apply => {
                if self.can_apply(raw, saved) {
                    effects.push(Effect::Save(Some(current.into())));
                }
                return effects;
            }
            Action::Undo => {
                if let Some(previous) = &self.replaced {
                    effects.push(Effect::Save(previous.clone()));
                }
                return effects;
            }
            Action::Ask(prompt) => {
                let prompt = prompt.trim().to_owned();
                if prompt.is_empty() {
                    self.editing.as_mut().unwrap().note = Some("Describe the change first.".into());
                } else {
                    self.last_ask = Some(prompt);
                    self.start_request(&mut effects);
                }
                return effects;
            }
            Action::Retry => {
                self.start_request(&mut effects);
                return effects;
            }
            Action::Cancel => {
                self.editing.as_mut().unwrap().note = Some("Request cancelled.".into());
                return effects;
            }
            _ => return effects,
        };
        match candidate {
            Ok(candidate) => self.stage(candidate, None),
            Err(error) => self.editing.as_mut().unwrap().note = Some(error),
        }
        effects
    }

    fn start_request(&mut self, effects: &mut Vec<Effect>) {
        let Some(prompt) = self.last_ask.clone() else {
            return;
        };
        let Some(request) = self.requests.allocate() else {
            self.editing.as_mut().unwrap().note =
                Some("No more preview requests are available.".into());
            return;
        };
        let e = self.editing.as_mut().unwrap();
        e.interpretation = Interpretation::Running {
            request,
            revision: e.revision,
        };
        e.note = None;
        effects.push(Effect::Ask(Ask {
            request,
            prompt,
            current: e.candidate.clone(),
        }));
    }

    pub fn receive(&mut self, request: u64, outcome: Outcome) -> bool {
        let Some(e) = &self.editing else {
            return false;
        };
        if e.interpretation
            != (Interpretation::Running {
                request,
                revision: e.revision,
            })
        {
            return false;
        }
        let current = e.candidate.clone();
        self.editing.as_mut().unwrap().interpretation = Interpretation::Idle;
        match outcome {
            Outcome::Proposal(Proposal::Change(candidate)) => match self
                .remap(&candidate, Some(&current))
            {
                Ok(candidate) => {
                    self.stage(candidate, None);
                    return true;
                }
                Err(error) => {
                    self.editing.as_mut().unwrap().interpretation = Interpretation::Failed(error)
                }
            },
            Outcome::Proposal(Proposal::Unsupported(reason)) => {
                self.editing.as_mut().unwrap().interpretation = Interpretation::Unsupported(reason)
            }
            Outcome::Failed(error) => {
                self.editing.as_mut().unwrap().interpretation = Interpretation::Failed(error)
            }
        }
        false
    }

    pub fn borrowed(
        &mut self,
        source: &ProjectId,
        layout: Option<&SavedComposition>,
    ) -> Vec<Effect> {
        let Some(layout) = layout.and_then(SavedComposition::supported) else {
            self.editing.as_mut().unwrap().chooser = Some(Chooser::Unavailable);
            return vec![];
        };
        let candidate = match self.remap(layout, None) {
            Ok(candidate) => candidate,
            Err(error) => {
                self.editing.as_mut().unwrap().note = Some(error);
                return vec![];
            }
        };
        let mut effects = Vec::new();
        self.cancel(&mut effects);
        self.stage(
            candidate,
            Some(format!("Borrowed from {}", source.path().display())),
        );
        self.editing.as_mut().unwrap().chooser = None;
        effects
    }

    pub fn saved(
        &mut self,
        previous: Option<SavedComposition>,
        shown: Composition,
        result: Result<bool, String>,
    ) {
        match result {
            Ok(warning) => {
                self.replaced = Some(previous);
                self.stage(
                    shown,
                    warning.then(|| "Layout applied, but not confirmed on disk.".into()),
                );
            }
            Err(error) => self.editing.as_mut().unwrap().note = Some(format!("Not saved. {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keystone_core::{Axis, Place};
    fn moved(c: &Composition) -> Edit {
        Edit::Move {
            view: ViewId(2),
            to: Place::Beside {
                target: c
                    .views()
                    .find(|(_, k)| *k == Component::Conversation)
                    .unwrap()
                    .0,
                axis: Axis::Row,
                after: true,
            },
        }
    }
    #[test]
    fn direct_edits_cancel_old_proposals_and_history_restores_absence() {
        let saved = Composition::default();
        let mut e = Customization::new(&saved, RequestIds::default());
        e.dispatch(Action::Open, None, &saved);
        e.dispatch(Action::Ask("right".into()), None, &saved);
        let request = e.running().unwrap();
        assert_eq!(
            e.dispatch(Action::Edit(moved(&saved)), None, &saved),
            vec![Effect::Cancel(request)]
        );
        let candidate = e.shown(&saved).clone();
        e.receive(request, Outcome::Proposal(Proposal::Change(saved.clone())));
        assert_eq!(e.shown(&saved), &candidate);
        e.saved(None, candidate.clone(), Err("disk full".into()));
        assert!(e.can_apply(None, &saved));
        assert!(!e.can_undo(&saved));
        e.saved(None, candidate.clone(), Ok(true));
        assert!(!e.can_apply(Some(&candidate.clone().into()), &candidate));
        assert!(!e.can_discard(&candidate));
        assert_eq!(
            e.dispatch(Action::Undo, Some(&candidate.clone().into()), &candidate),
            vec![Effect::Save(None)]
        );
    }
    #[test]
    fn borrowed_occurrences_never_alias_retained_views_or_modify_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let source = ProjectId::new(dir.path()).unwrap();
        let saved = Composition::default();
        let raw = SavedComposition::from(saved.clone());
        let mut e = Customization::new(&saved, RequestIds::default());
        e.dispatch(Action::Open, None, &saved);
        e.dispatch(Action::Duplicate(ViewId::MAIN), None, &saved);
        let retained: Vec<_> = e.shown(&saved).views().collect();
        let candidate = e.shown(&saved).clone();
        e.borrowed(&source, None);
        assert_eq!(e.shown(&saved), &candidate);
        e.borrowed(&source, Some(&raw));
        assert!(
            e.shown(&saved)
                .views()
                .all(|(id, _)| retained.iter().all(|(old, _)| *old != id))
        );
        e.dispatch(Action::Discard, None, &saved);
        e.dispatch(Action::Duplicate(ViewId::MAIN), None, &saved);
        assert!(e.shown(&saved).views().any(|(id, _)| id.0 > 6));
    }
    #[test]
    fn chooser_cancel_preserves_a_preview_and_old_request_cannot_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let source = ProjectId::new(dir.path()).unwrap();
        let saved = Composition::default();
        let mut e = Customization::new(&saved, RequestIds::default());
        e.dispatch(Action::Open, None, &saved);
        e.dispatch(Action::Edit(moved(&saved)), None, &saved);
        let candidate = e.shown(&saved).clone();
        e.dispatch(Action::Borrow, None, &saved);
        e.dispatch(Action::Choose(source), None, &saved);
        assert!(e.dispatch(Action::PreviewBorrow, None, &saved).is_empty());
        assert!(matches!(e.chooser(), Some(Chooser::Replace(_))));
        e.dispatch(Action::CloseChooser, None, &saved);
        assert_eq!(e.shown(&saved), &candidate);
        assert!(e.dispatch(Action::Undo, None, &saved).is_empty());
        e.dispatch(Action::Ask("right".into()), None, &saved);
        let request = e.running().unwrap();
        e.dispatch(Action::Close, None, &saved);
        e.dispatch(Action::Open, None, &saved);
        e.receive(request, Outcome::Proposal(Proposal::Change(candidate)));
        assert_eq!(e.shown(&saved), &saved);
    }
}
