use super::*;
use crate::FOLDER_UNCONFIRMED;
use crate::arrangement_editor::{Action, Commit};
use crate::customization_agent::{Outcome, Proposal};
use crate::view_state::ViewKey;
use gpui::{ListOffset, TestAppContext, VisualTestContext, px};
use keystone_agent::Event;
use keystone_core::{ApprovalMode, ContextOrigin, Provider, Store, ViewId};
use keystone_ui::Color;
use std::fs::File;
use std::path::Path;
use std::rc::Rc;

fn open(
    cx: &mut TestAppContext,
    contexts: bool,
) -> (tempfile::TempDir, Entity<Keystone>, &mut VisualTestContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("a")).unwrap();
    std::fs::create_dir(dir.path().join("b")).unwrap();
    std::fs::create_dir(dir.path().join("state")).unwrap();
    let path = dir.path().join("state/workspace.json");
    let mut store = Store::open(&path).unwrap();
    let directory = WorkingDirectory::new(&dir.path().join("a")).unwrap();
    store.create(Provider::Codex, &directory, 1).unwrap();
    store.create(Provider::Claude, &directory, 2).unwrap();
    store.select(1).unwrap();
    if !contexts {
        drop(store);
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["workspace"]["sessions"][0]
            .as_object_mut()
            .unwrap()
            .remove("execution_context");
        value["workspace"]["sessions"][1]["execution_context"] =
            serde_json::json!({"version": 700, "extra": [null, "keep"]});
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        store = Store::open(&path).unwrap();
    }
    cx.update(keystone_ui::init);
    cx.executor().allow_parking();
    let (view, cx) = cx.add_window_view(|window, cx| Keystone::new(store, false, window, cx));
    pick(&view, cx, &dir.path().join("a"));
    (dir, view, cx)
}
fn pick(view: &Entity<Keystone>, cx: &mut VisualTestContext, directory: &Path) {
    view.update(cx, |this, cx| {
        let ticket = this.presentation.project_ticket().unwrap();
        this.finish_project_pick(ticket, FolderPick::Selected(directory.to_owned()), cx);
    });
    cx.run_until_parked();
}
fn key() -> ViewKey {
    ViewKey {
        view: ViewId::MAIN,
        session: 1,
    }
}

#[gpui::test]
fn project_return_retains_editor_prompt_undo_and_offscreen_transcript_changes(
    cx: &mut TestAppContext,
) {
    let (dir, view, cx) = open(cx, true);
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    let (activation, candidate, prompt) = view.update(cx, |this, cx| {
        let saved = this.saved_composition();
        this.presentation
            .current
            .customization
            .dispatch(Action::Open, None, &saved);
        this.presentation.current.customization.dispatch(
            Action::Duplicate(ViewId::MAIN),
            None,
            &saved,
        );
        let candidate = this
            .presentation
            .current
            .customization
            .shown(&saved)
            .clone();
        this.store
            .set_project_layout(
                this.presentation.project.as_ref().unwrap(),
                Some(candidate.clone().into()),
            )
            .unwrap();
        this.presentation.current.customization.saved(
            Commit::Apply,
            None,
            candidate.clone(),
            Ok(false),
        );
        this.presentation
            .current
            .customize_prompt
            .update(cx, |i, cx| i.set_value("Keep A prompt".into(), cx));
        let prompt = this.presentation.current.customize_prompt.clone();
        for i in 0..40 {
            this.conversation(1).push_local(&format!(
                "Retained row {i}. {}",
                "Content for measured scroll. ".repeat(8)
            ));
        }
        this.commit(1);
        this.presentation
            .current
            .views
            .list(key(), 40)
            .scroll_to(ListOffset {
                item_ix: 7,
                offset_in_item: px(19.),
            });
        (this.presentation.activation(), candidate, prompt)
    });
    pick(&view, cx, &b);
    view.update_in(cx, |this, window, cx| {
        assert!(!this.presentation.matches(activation));
        assert_eq!(
            this.presentation.current.customize_prompt.read(cx).value(),
            ""
        );
        this.customize(Action::Open, window, cx);
        this.customize(Action::Duplicate(ViewId::MAIN), window, cx);
        this.customize(Action::Apply, window, cx);
        this.presentation
            .current
            .customize_prompt
            .update(cx, |i, cx| i.set_value("B prompt".into(), cx));
        this.conversation(1)
            .push_local("Appended while A is parked.");
        this.commit(1);
    });
    pick(&view, cx, &a);
    view.read_with(cx, |this, cx| {
        assert!(!this.presentation.matches(activation));
        assert_eq!(
            this.presentation.current.customize_prompt.entity_id(),
            prompt.entity_id()
        );
        assert_eq!(
            this.presentation.current.customize_prompt.read(cx).value(),
            "Keep A prompt"
        );
        assert_eq!(
            this.presentation
                .current
                .customization
                .shown(&this.saved_composition()),
            &candidate
        );
        assert!(
            this.presentation
                .current
                .customization
                .can_undo(&this.saved_composition())
        );
        let list = this.presentation.current.views.get(key()).unwrap();
        assert_eq!(list.item_count(), 42);
        assert_eq!(list.logical_scroll_top().item_ix, 7);
        assert_eq!(list.logical_scroll_top().offset_in_item, px(19.));
    });
    view.update_in(cx, |this, window, cx| {
        this.customize(Action::Undo, window, cx)
    });
    view.read_with(cx, |this, _| {
        assert!(
            this.store
                .workspace()
                .preferences()
                .project_layout(&ProjectId::new(&a).unwrap())
                .is_none()
        );
        assert!(
            this.store
                .workspace()
                .preferences()
                .project_layout(&ProjectId::new(&b).unwrap())
                .is_some()
        );
    });
    pick(&view, cx, &b);
    view.read_with(cx, |this, cx| {
        assert_eq!(
            this.presentation.current.customize_prompt.read(cx).value(),
            "B prompt"
        )
    });
}

#[gpui::test]
fn newest_project_picker_and_same_canonical_alias_preserve_current_work(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(cx, true);
    let b = dir.path().join("b");
    std::os::unix::fs::symlink(dir.path().join("a"), dir.path().join("alias")).unwrap();
    view.update(cx, |this, cx| {
        let saved = this.saved_composition();
        this.presentation
            .current
            .customization
            .dispatch(Action::Open, None, &saved);
        this.presentation.current.customization.dispatch(
            Action::Ask {
                provider: Provider::Claude,
                prompt: "Retain".into(),
            },
            None,
            &saved,
        );
        this.composer
            .update(cx, |i, cx| i.set_value("Current draft".into(), cx));
        let running = this.presentation.current.customization.running();
        let activation = this.presentation.activation();
        let first = this.presentation.project_ticket().unwrap();
        let newest = this.presentation.project_ticket().unwrap();
        this.finish_project_pick(first, FolderPick::Selected(b.clone()), cx);
        assert_eq!(this.presentation.activation(), activation);
        this.finish_project_pick(newest, FolderPick::Cancelled, cx);
        this.finish_project_pick(first, FolderPick::Failed("stale error".into()), cx);
        assert!(this.error.is_none());
        let same = this.presentation.project_ticket().unwrap();
        this.finish_project_pick(same, FolderPick::Selected(dir.path().join("alias")), cx);
        assert_eq!(this.presentation.activation(), activation);
        assert_eq!(this.presentation.current.customization.running(), running);
        assert_eq!(this.store.workspace().sessions()[0].draft(), "");
        assert_eq!(this.composer.read(cx).value(), "Current draft");
        assert!(
            this.store
                .workspace()
                .preferences()
                .project_layout(this.presentation.project.as_ref().unwrap())
                .is_none()
        );
    });
}

#[gpui::test]
fn failed_draft_barrier_keeps_project_preview_and_helper_acceptance(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(cx, true);
    view.update(cx, |this, cx| {
        let saved = this.saved_composition();
        this.presentation
            .current
            .customization
            .dispatch(Action::Open, None, &saved);
        this.presentation.current.customization.dispatch(
            Action::Ask {
                provider: Provider::Claude,
                prompt: "Keep running".into(),
            },
            None,
            &saved,
        );
        let request = this.presentation.current.customization.running().unwrap();
        let activation = this.presentation.activation();
        this.composer
            .update(cx, |i, cx| i.set_value("Unsaved now".into(), cx));
        std::fs::rename(dir.path().join("state"), dir.path().join("backup")).unwrap();
        std::fs::write(dir.path().join("state"), b"blocks parent").unwrap();
        let ticket = this.presentation.project_ticket().unwrap();
        this.finish_project_pick(ticket, FolderPick::Selected(dir.path().join("b")), cx);
        assert_eq!(this.presentation.activation(), activation);
        assert_eq!(
            this.presentation.current.customization.running(),
            Some(request)
        );
        assert_eq!(this.composer.read(cx).value(), "Unsaved now");
        assert!(this.error.as_ref().unwrap().contains("Draft not saved"));
        std::fs::remove_file(dir.path().join("state")).unwrap();
        std::fs::rename(dir.path().join("backup"), dir.path().join("state")).unwrap();
    });
}

#[gpui::test]
fn suspended_helpers_and_old_frame_mutations_stay_stale_after_return(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(cx, true);
    let (request, stale) = view.update(cx, |this, cx| {
        let saved = this.saved_composition();
        this.presentation
            .current
            .customization
            .dispatch(Action::Open, None, &saved);
        this.presentation.current.customization.dispatch(
            Action::Ask {
                provider: Provider::Claude,
                prompt: "A request".into(),
            },
            None,
            &saved,
        );
        (
            this.presentation.current.customization.running().unwrap(),
            this.scoped_listener(cx, |this, _: &(), _, _| {
                this.provider_filter = Some(Provider::Claude)
            }),
        )
    });
    pick(&view, cx, &dir.path().join("b"));
    view.update(cx, |this, _| {
        let saved = this.saved_composition();
        this.presentation
            .current
            .customization
            .dispatch(Action::Open, None, &saved);
        this.presentation.current.customization.dispatch(
            Action::Ask {
                provider: Provider::Claude,
                prompt: "B request".into(),
            },
            None,
            &saved,
        );
        assert!(this.presentation.current.customization.running().unwrap() > request);
        assert!(!this.presentation.current.customization.receive(
            request,
            Outcome::Proposal(Proposal::Change(Composition::default()))
        ));
    });
    pick(&view, cx, &dir.path().join("a"));
    cx.update(|window, cx| stale(&(), window, cx));
    view.update(cx, |this, _| {
        assert!(this.provider_filter.is_none());
        assert!(!this.presentation.current.customization.receive(
            request,
            Outcome::Proposal(Proposal::Change(Composition::default()))
        ));
        assert!(this.presentation.current.customization.can_retry());
    });
}

#[gpui::test]
fn legacy_ticket_is_invalid_after_selection_return_and_after_project_transition(
    cx: &mut TestAppContext,
) {
    let (dir, view, cx) = open(cx, false);
    view.update(cx, |this, cx| {
        let first = this.presentation.legacy_ticket(1).unwrap();
        this.select(2, cx);
        this.select(1, cx);
        this.finish_legacy_folder(first, FolderPick::Failed("stale error".into()), cx);
        this.finish_legacy_folder(first, FolderPick::Selected(dir.path().join("a")), cx);
        assert_eq!(
            this.conversation_folder(1),
            Some(ConversationFolder::LegacyMissing)
        );
        assert!(this.error.is_none());
        let older = this.presentation.legacy_ticket(1).unwrap();
        let latest = this.presentation.legacy_ticket(1).unwrap();
        this.finish_legacy_folder(latest, FolderPick::Cancelled, cx);
        this.finish_legacy_folder(older, FolderPick::Selected(dir.path().join("a")), cx);
        assert_eq!(
            this.conversation_folder(1),
            Some(ConversationFolder::LegacyMissing)
        );
        let ticket = this.presentation.legacy_ticket(1).unwrap();
        let project = this.presentation.project_ticket().unwrap();
        this.finish_project_pick(project, FolderPick::Selected(dir.path().join("b")), cx);
        this.finish_legacy_folder(ticket, FolderPick::Selected(dir.path().join("a")), cx);
        assert_eq!(
            this.conversation_folder(1),
            Some(ConversationFolder::LegacyMissing)
        );
    });
}

#[gpui::test]
fn legacy_choice_saves_without_send_and_unsupported_data_cannot_be_replaced(
    cx: &mut TestAppContext,
) {
    let (dir, view, cx) = open(cx, false);
    view.update_in(cx, |this, window, cx| {
        this.composer
            .update(cx, |i, cx| i.set_value("Explicit only".into(), cx));
        this.send_message(
            crate::ComposerDraft {
                session: 1,
                text: "Explicit only".into(),
            },
            window,
            cx,
        );
        assert!(this.codex.is_none());
        assert_eq!(this.composer.read(cx).value(), "Explicit only");
        assert!(this.conversation(1).rows.is_empty());
        let ticket = this.presentation.legacy_ticket(1).unwrap();
        this.select(1, cx);
        this.finish_legacy_folder(ticket, FolderPick::Selected(dir.path().join("a")), cx);
        assert!(matches!(
            this.store.workspace().sessions()[0].context(),
            SessionContext::Recorded {
                origin: ContextOrigin::Chosen,
                ..
            }
        ));
        assert_eq!(this.composer.read(cx).value(), "Explicit only");
        assert!(this.codex.is_none());
        assert!(this.conversation(1).rows.is_empty());
        this.select(2, cx);
        let before = std::fs::read(this.store.path()).unwrap();
        this.choose_legacy_folder(2, cx);
        this.send_message(
            crate::ComposerDraft {
                session: 2,
                text: "Never send unsupported".into(),
            },
            window,
            cx,
        );
        assert!(this.claude.is_none());
        assert_eq!(
            this.conversation_folder(2),
            Some(ConversationFolder::Unsupported)
        );
        assert_eq!(std::fs::read(this.store.path()).unwrap(), before);
    });
}

#[gpui::test]
fn full_access_callbacks_require_the_exact_confirmation(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(cx, true);
    view.update_in(cx, |this, window, cx| {
        this.choose_approval_mode(keystone_core::ApprovalMode::FullAccess, window, cx);
        let old = this.confirm_full_access.as_ref().unwrap().identity.clone();
        this.confirm_full_access = None;
        this.choose_approval_mode(keystone_core::ApprovalMode::FullAccess, window, cx);
        let latest = this.confirm_full_access.as_ref().unwrap().identity.clone();
        assert!(this.take_full_access_confirmation(&old).is_none());
        assert!(Rc::ptr_eq(
            &this.confirm_full_access.as_ref().unwrap().identity,
            &latest
        ));
        let ticket = this.presentation.project_ticket().unwrap();
        this.finish_project_pick(ticket, FolderPick::Selected(dir.path().join("b")), cx);
        assert!(this.take_full_access_confirmation(&latest).is_none());
        assert_eq!(
            this.store.workspace().sessions()[0].approval_mode(),
            keystone_core::ApprovalMode::Provider
        );
    });
}

#[gpui::test]
fn unavailable_recorded_directory_never_uses_the_layout_project(cx: &mut TestAppContext) {
    use std::os::unix::fs::PermissionsExt;
    let (dir, view, cx) = open(cx, true);
    let a = dir.path().join("a");
    pick(&view, cx, &dir.path().join("b"));
    for removed in [false, true] {
        if removed {
            std::fs::remove_dir(&a).unwrap();
        } else {
            std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o000)).unwrap();
        }
        view.update_in(cx, |this, window, cx| {
            this.composer
                .update(cx, |i, cx| i.set_value("Keep unavailable draft".into(), cx));
            this.send_message(
                crate::ComposerDraft {
                    session: 1,
                    text: "Keep unavailable draft".into(),
                },
                window,
                cx,
            );
            assert!(this.codex.is_none());
            assert!(this.conversation(1).rows.is_empty());
            assert_eq!(this.composer.read(cx).value(), "Keep unavailable draft");
            assert!(
                this.conversation(1)
                    .error
                    .as_ref()
                    .unwrap()
                    .contains("saved folder is unavailable")
            );
            let Some(ConversationFolder::Recorded(directory)) = this.conversation_folder(1) else {
                panic!()
            };
            assert_eq!(directory.path(), a);
        });
        if !removed {
            std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
}

#[gpui::test]
fn creation_after_project_switch_records_b_and_keeps_existing_a_context(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(cx, true);
    view.update(cx, |this, cx| {
        this.composer
            .update(cx, |input, cx| input.set_value("A unsent draft".into(), cx));
    });
    pick(&view, cx, &dir.path().join("b"));
    view.update(cx, |this, cx| {
        this.create(Provider::Claude, cx);
        assert_eq!(this.store.workspace().selected(), Some(3));
        assert_eq!(
            this.store.workspace().sessions()[0].draft(),
            "A unsent draft"
        );
        for (session, folder) in [(1, "a"), (3, "b")] {
            let Some(ConversationFolder::Recorded(directory)) = this.conversation_folder(session)
            else {
                panic!()
            };
            assert_eq!(directory.path(), dir.path().join(folder));
        }
        assert!(matches!(
            this.store.workspace().sessions()[2].context(),
            SessionContext::Recorded {
                origin: ContextOrigin::Created,
                ..
            }
        ));
        assert_eq!(this.composer.read(cx).value(), "");
        assert!(this.codex.is_none() && this.claude.is_none());
    });
}

fn refuse_directory_sync(this: &mut Keystone) {
    this.store.sync_directory = |_| Err(std::io::Error::other("directory sync unavailable"));
}

#[gpui::test]
fn folder_success_keeps_an_unresolved_provider_thread_error(cx: &mut TestAppContext) {
    use std::os::unix::fs::PermissionsExt;
    let (dir, view, cx) = open(cx, false);
    let state = dir.path().join("state");
    view.update(cx, |this, cx| {
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o555)).unwrap();
        let ready = Event::ThreadReady {
            thread_id: "rejected-thread".into(),
        };
        this.receive(vec![], vec![(1, ready)], cx);
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();
        let unresolved = |this: &Keystone| {
            this.error
                .as_deref()
                .is_some_and(|e| e.starts_with("Could not save the conversation ID"))
        };
        assert!(unresolved(this), "{:?}", this.error);
        let ticket = this.presentation.legacy_ticket(1).unwrap();
        this.finish_legacy_folder(ticket, FolderPick::Selected(dir.path().join("a")), cx);
        assert!(matches!(
            this.conversation_folder(1),
            Some(ConversationFolder::Recorded(_))
        ));
        assert!(unresolved(this), "{:?}", this.error);
        let saved = std::fs::read_to_string(this.store.path()).unwrap();
        assert!(saved.contains(r#""origin":"chosen""#), "{saved}");
        assert!(!saved.contains("rejected-thread"), "{saved}");
    });
}

#[gpui::test]
fn applied_folder_warning_ends_with_confirmation_and_never_returns(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(cx, false);
    view.update_in(cx, |this, window, cx| {
        refuse_directory_sync(this);
        let ticket = this.presentation.legacy_ticket(1).unwrap();
        this.finish_legacy_folder(ticket, FolderPick::Selected(dir.path().join("a")), cx);
        assert_eq!(
            this.status_line(),
            Some((FOLDER_UNCONFIRMED.into(), Color::Warning))
        );

        this.store.sync_directory = File::sync_all;
        this.customize(Action::Open, window, cx);
        this.customize(Action::Duplicate(ViewId::MAIN), window, cx);
        this.customize(Action::Apply, window, cx);
        assert!(this.saved_layout().is_some());
        assert_eq!(this.status_line(), None, "confirmed by the unrelated Apply");

        refuse_directory_sync(this);
        this.set_approval_mode(1, ApprovalMode::ReadOnly, window, cx);
        let (later, color) = this.status_line().unwrap();
        assert!(
            later.starts_with("Change saved, but crash durability"),
            "{later}"
        );
        assert_eq!(color, Color::Danger);

        this.store.sync_directory = File::sync_all;
        this.composer
            .update(cx, |i, cx| i.set_value("typed after".into(), cx));
        assert!(this.save_draft(cx));
        assert_eq!(
            this.status_line(),
            None,
            "the folder warning does not return"
        );
    });
}

#[gpui::test]
fn send_with_unrelated_save_debt_refuses_without_blaming_a_folder(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open(cx, true);
    view.update_in(cx, |this, window, cx| {
        refuse_directory_sync(this);
        this.set_approval_mode(1, ApprovalMode::ReadOnly, window, cx);
        this.composer
            .update(cx, |i, cx| i.set_value("Retry later".into(), cx));
        this.send_message(
            crate::ComposerDraft {
                session: 1,
                text: "Retry later".into(),
            },
            window,
            cx,
        );
        assert!(this.codex.is_none());
        assert_eq!(this.composer.read(cx).value(), "Retry later");
        assert!(this.conversation(1).rows.is_empty());
        assert_eq!(
            this.conversation(1).error.as_deref(),
            Some(
                "Not sent. Could not finish saving changes. Your draft is kept. Send will retry saving. directory sync unavailable"
            )
        );
    });
}
