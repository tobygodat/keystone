use super::*;
use crate::arrangement_editor::Action;
use crate::customization_agent::{Outcome, Proposal};
use crate::model_selector::{CatalogKey, Choices};
use crate::view_state::ViewKey;
use gpui::{ListOffset, TestAppContext, VisualTestContext, px};
use keystone_agent::Event;
use keystone_core::{ApprovalMode, ContextOrigin, Provider, SessionContext, Store, ViewId};
use keystone_ui::Color;
use std::fs::File;
use std::path::Path;
use std::rc::Rc;

fn open(
    cx: &mut TestAppContext,
    contexts: bool,
) -> (tempfile::TempDir, Entity<Keystone>, &mut VisualTestContext) {
    // Canonical, like the folders Keystone records: macOS's temporary folder sits
    // behind a symlink.
    let dir = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    std::fs::create_dir(dir.path().join("a")).unwrap();
    std::fs::create_dir(dir.path().join("b")).unwrap();
    std::fs::create_dir(dir.path().join("state")).unwrap();
    let path = dir.path().join("state/workspace.json");
    let mut store = Store::in_memory().unwrap();
    let directory = WorkingDirectory::new(&dir.path().join("a")).unwrap();
    store.create(Provider::Codex, &directory, 1).unwrap();
    store.create(Provider::Claude, &directory, 2).unwrap();
    store.select(1).unwrap();
    let mut value = store.saved_snapshot().unwrap();
    if !contexts {
        value["workspace"]["sessions"][0]
            .as_object_mut()
            .unwrap()
            .remove("execution_context");
        value["workspace"]["sessions"][1]["execution_context"] =
            serde_json::json!({"version": 700, "extra": [null, "keep"]});
    }
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
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
    pick(&view, cx, &dir.path().join("a"));
    view.update_in(cx, |this, window, cx| this.focus.focus(window, cx));
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
fn project_switch_releases_a_transcript_copy_action_and_restores_its_row(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(cx, true);
    cx.update(|_, cx| crate::bind_keys(cx));
    view.update(cx, |this, cx| {
        let mut conversation = crate::Conversation::default();
        conversation.push_local("project-local keyboard row");
        this.state.update(cx, |state, _| {
            state.test_conversations_mut().insert(1, conversation)
        });
        cx.notify();
    });
    cx.simulate_keystrokes("secondary-l");
    cx.simulate_input("draft kept across the switch");
    cx.simulate_keystrokes("secondary-shift-t home tab");
    let old_focus = view.update_in(cx, |this, window, _| {
        let focus = this
            .presentation
            .current
            .views
            .keyboard_state(key())
            .unwrap()
            .action_focus
            .clone();
        assert!(focus.is_focused(window));
        focus
    });
    pick(&view, cx, &dir.path().join("b"));
    cx.update(|window, _| assert!(!old_focus.is_focused(window)));
    view.read_with(cx, |this, cx| {
        assert_eq!(this.selected(cx), None, "B has no session yet");
        assert_eq!(this.composer.read(cx).value(), "");
    });
    pick(&view, cx, &dir.path().join("a"));
    cx.simulate_keystrokes("secondary-shift-t secondary-shift-c");
    let text = cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()));
    assert_eq!(text.as_deref(), Some("project-local keyboard row"));
    view.read_with(cx, |this, cx| {
        assert_eq!(
            this.composer.read(cx).value(),
            "draft kept across the switch"
        )
    });
}

#[gpui::test]
fn project_return_retains_editor_prompt_undo_and_offscreen_transcript_changes(
    cx: &mut TestAppContext,
) {
    let (dir, view, cx) = open(cx, true);
    view.update(cx, |this, cx| {
        this.state
            .update(cx, |state, cx| state.flush_logs(cx).unwrap());
    });
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    let (activation, candidate, prompt) = view.update(cx, |this, cx| {
        let saved = this.saved_composition(cx);
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
        this.state
            .update(cx, |state, cx| {
                state.set_project_layout(
                    this.presentation.project.as_ref().unwrap(),
                    Some(candidate.clone().into()),
                    cx,
                )
            })
            .unwrap();
        this.presentation
            .current
            .customization
            .saved(None, candidate.clone(), Ok(()));
        this.presentation
            .current
            .customize_prompt
            .update(cx, |i, cx| i.set_value("Keep A prompt".into(), cx));
        let prompt = this.presentation.current.customize_prompt.clone();
        for i in 0..40 {
            this.state.update(cx, |state, _| {
                state.test_conversation(1).push_local(&format!(
                    "Retained row {i}. {}",
                    "Content for measured scroll. ".repeat(8)
                ))
            });
        }
        this.commit(1, cx);
        (this.presentation.activation(), candidate, prompt)
    });
    view.update(cx, |this, _| {
        this.presentation
            .current
            .views
            .list(key(), 40)
            .scroll_to(ListOffset {
                item_ix: 7,
                offset_in_item: px(19.),
            });
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
        this.state.update(cx, |state, _| {
            state
                .test_conversation(1)
                .push_local("Appended while A is parked.")
        });
        this.commit(1, cx);
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
                .shown(&this.saved_composition(cx)),
            &candidate
        );
        assert!(
            this.presentation
                .current
                .customization
                .can_undo(&this.saved_composition(cx))
        );
        let list = this.presentation.current.views.get(key()).unwrap();
        assert_eq!(list.item_count(), 42);
        assert_eq!(list.logical_scroll_top().item_ix, 7);
        assert_eq!(list.logical_scroll_top().offset_in_item, px(19.));
    });
    view.update_in(cx, |this, window, cx| {
        this.customize(Action::Undo, window, cx)
    });
    view.read_with(cx, |this, cx| {
        assert!(
            this.state
                .read(cx)
                .store()
                .workspace()
                .preferences()
                .project_layout(&ProjectId::new(&a).unwrap())
                .is_none()
        );
        assert!(
            this.state
                .read(cx)
                .store()
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
        let saved = this.saved_composition(cx);
        this.presentation
            .current
            .customization
            .dispatch(Action::Open, None, &saved);
        this.presentation.current.customization.dispatch(
            Action::Ask("Retain".into()),
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
        assert_eq!(
            this.state.read(cx).store().workspace().sessions()[0].draft(),
            ""
        );
        assert_eq!(this.composer.read(cx).value(), "Current draft");
        assert!(
            this.state
                .read(cx)
                .store()
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
        let saved = this.saved_composition(cx);
        this.presentation
            .current
            .customization
            .dispatch(Action::Open, None, &saved);
        this.presentation.current.customization.dispatch(
            Action::Ask("Keep running".into()),
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
        assert_eq!(this.presentation.activation().0, activation.0 + 1);
        assert_eq!(
            this.presentation.current.customization.running(),
            None,
            "{request:?} stays with the project it was asked in"
        );
        assert_eq!(
            this.state.read(cx).store().workspace().sessions()[0].draft(),
            "Unsaved now"
        );
        settle(this, cx);
        assert!(this.status_line(cx).unwrap().0.starts_with("Not saved."));
        std::fs::remove_file(dir.path().join("state")).unwrap();
        std::fs::rename(dir.path().join("backup"), dir.path().join("state")).unwrap();
    });
}

#[gpui::test]
fn suspended_helpers_and_old_frame_mutations_stay_stale_after_return(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(cx, true);
    let (request, stale) = view.update(cx, |this, cx| {
        let saved = this.saved_composition(cx);
        this.presentation
            .current
            .customization
            .dispatch(Action::Open, None, &saved);
        this.presentation.current.customization.dispatch(
            Action::Ask("A request".into()),
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
    view.update(cx, |this, cx| {
        let saved = this.saved_composition(cx);
        this.presentation
            .current
            .customization
            .dispatch(Action::Open, None, &saved);
        this.presentation.current.customization.dispatch(
            Action::Ask("B request".into()),
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
    view.update(cx, |this, _cx| {
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
            this.conversation_folder(1, cx),
            Some(ConversationFolder::LegacyMissing)
        );
        assert!(this.error.is_none());
        let older = this.presentation.legacy_ticket(1).unwrap();
        let latest = this.presentation.legacy_ticket(1).unwrap();
        this.finish_legacy_folder(latest, FolderPick::Cancelled, cx);
        this.finish_legacy_folder(older, FolderPick::Selected(dir.path().join("a")), cx);
        assert_eq!(
            this.conversation_folder(1, cx),
            Some(ConversationFolder::LegacyMissing)
        );
        let ticket = this.presentation.legacy_ticket(1).unwrap();
        let project = this.presentation.project_ticket().unwrap();
        this.finish_project_pick(project, FolderPick::Selected(dir.path().join("b")), cx);
        this.finish_legacy_folder(ticket, FolderPick::Selected(dir.path().join("a")), cx);
        assert_eq!(
            this.conversation_folder(1, cx),
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
                attachments: Vec::new(),
            },
            window,
            cx,
        );
        assert!(this.state.read(cx).test_codex().is_none());
        assert_eq!(this.composer.read(cx).value(), "Explicit only");
        assert!(this.state.read(cx).conversations()[&1].rows.is_empty());
        let ticket = this.presentation.legacy_ticket(1).unwrap();
        this.select(1, cx);
        this.finish_legacy_folder(ticket, FolderPick::Selected(dir.path().join("a")), cx);
        assert!(matches!(
            this.state.read(cx).store().workspace().sessions()[0].context(),
            SessionContext::Recorded {
                origin: ContextOrigin::Chosen,
                ..
            }
        ));
        assert_eq!(this.composer.read(cx).value(), "Explicit only");
        assert!(this.state.read(cx).test_codex().is_none());
        assert!(this.state.read(cx).conversations()[&1].rows.is_empty());
        this.select(2, cx);
        let before = this.state.read(cx).store().saved_snapshot().unwrap();
        this.choose_legacy_folder(2, cx);
        this.send_message(
            crate::ComposerDraft {
                session: 2,
                text: "Never send unsupported".into(),
                attachments: Vec::new(),
            },
            window,
            cx,
        );
        assert!(this.state.read(cx).test_claude().is_none());
        assert_eq!(
            this.conversation_folder(2, cx),
            Some(ConversationFolder::Unsupported)
        );
        assert_eq!(
            this.state.read(cx).store().saved_snapshot().unwrap(),
            before
        );
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
        assert!(this.take_full_access_confirmation(&old, cx).is_none());
        assert!(Rc::ptr_eq(
            &this.confirm_full_access.as_ref().unwrap().identity,
            &latest
        ));
        let ticket = this.presentation.project_ticket().unwrap();
        this.finish_project_pick(ticket, FolderPick::Selected(dir.path().join("b")), cx);
        assert!(this.take_full_access_confirmation(&latest, cx).is_none());
        assert_eq!(
            this.state.read(cx).store().workspace().sessions()[0].approval_mode(),
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
                    attachments: Vec::new(),
                },
                window,
                cx,
            );
            assert!(this.state.read(cx).test_codex().is_none());
            assert!(this.state.read(cx).conversations()[&1].rows.is_empty());
            assert_eq!(this.composer.read(cx).value(), "Keep unavailable draft");
            assert!(
                this.state.read(cx).conversations()[&1]
                    .error
                    .as_ref()
                    .unwrap()
                    .contains("saved folder is unavailable")
            );
            let Some(ConversationFolder::Recorded(directory)) = this.conversation_folder(1, cx)
            else {
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
        this.create(Provider::Claude, None, cx);
        assert_eq!(this.state.read(cx).store().workspace().selected(), Some(3));
        assert_eq!(
            this.state.read(cx).store().workspace().sessions()[0].draft(),
            "A unsent draft"
        );
        for (session, folder) in [(1, "a"), (3, "b")] {
            let Some(ConversationFolder::Recorded(directory)) =
                this.conversation_folder(session, cx)
            else {
                panic!()
            };
            assert_eq!(directory.path(), dir.path().join(folder));
        }
        assert!(matches!(
            this.state.read(cx).store().workspace().sessions()[2].context(),
            SessionContext::Recorded {
                origin: ContextOrigin::Created,
                ..
            }
        ));
        assert_eq!(this.composer.read(cx).value(), "");
        assert!(
            this.state.read(cx).test_codex().is_none()
                && this.state.read(cx).test_claude().is_none()
        );
    });
}

fn refuse_directory_sync(this: &mut Keystone, cx: &mut Context<Keystone>) {
    this.state.update(cx, |state, _| {
        state.test_sync_directory(|_| Err(std::io::Error::other("directory sync unavailable")));
    });
}

fn settle(this: &mut Keystone, cx: &mut Context<Keystone>) {
    this.state.update(cx, |state, cx| {
        let _ = state.store().flush();
        state.pump(cx);
    });
}

#[gpui::test]
fn folder_success_keeps_an_unresolved_provider_thread_error(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(cx, false);
    let state = dir.path().join("state");
    view.update(cx, |this, cx| {
        let writer = crate::tests::lock_database(&state.join("workspace.sqlite3"));
        let ready = Event::ThreadReady {
            thread_id: "rejected-thread".into(),
        };
        this.receive(vec![], vec![(1, ready)], cx);
        settle(this, cx);
        drop(writer);
        let unresolved = |this: &Keystone, cx: &App| {
            this.state
                .read(cx)
                .error
                .as_deref()
                .is_some_and(|e| e.starts_with("Not saved. database is locked"))
        };
        assert!(unresolved(this, cx), "{:?}", this.error);
        let ticket = this.presentation.legacy_ticket(1).unwrap();
        this.finish_legacy_folder(ticket, FolderPick::Selected(dir.path().join("a")), cx);
        assert!(matches!(
            this.conversation_folder(1, cx),
            Some(ConversationFolder::Recorded(_))
        ));
        settle(this, cx);
        assert!(!unresolved(this, cx), "{:?}", this.error);
        let saved = this
            .state
            .read(cx)
            .store()
            .saved_snapshot()
            .unwrap()
            .to_string();
        assert!(saved.contains(r#""origin":"chosen""#), "{saved}");
        assert!(saved.contains("rejected-thread"), "{saved}");
    });
}

#[gpui::test]
fn applied_folder_warning_ends_with_confirmation_and_never_returns(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(cx, false);
    view.update_in(cx, |this, window, cx| {
        refuse_directory_sync(this, cx);
        let ticket = this.presentation.legacy_ticket(1).unwrap();
        this.finish_legacy_folder(ticket, FolderPick::Selected(dir.path().join("a")), cx);
        settle(this, cx);
        let (warning, color) = this.status_line(cx).unwrap();
        assert_eq!(
            warning,
            "Saved, but not confirmed on disk. directory sync unavailable"
        );
        assert_eq!(color, Color::Warning);

        this.state.update(cx, |state, _| {
            state.test_sync_directory(File::sync_all);
        });
        this.customize(Action::Open, window, cx);
        this.customize(Action::Duplicate(ViewId::MAIN), window, cx);
        this.customize(Action::Apply, window, cx);
        assert!(this.saved_layout(cx).is_some());
        settle(this, cx);
        assert_eq!(
            this.status_line(cx),
            None,
            "confirmed by the unrelated Apply"
        );

        refuse_directory_sync(this, cx);
        this.set_approval_mode(1, ApprovalMode::ReadOnly, window, cx);
        settle(this, cx);
        let (later, color) = this.status_line(cx).unwrap();
        assert!(
            later.starts_with("Saved, but not confirmed on disk"),
            "{later}"
        );
        assert_eq!(color, Color::Warning);

        this.state.update(cx, |state, _| {
            state.test_sync_directory(File::sync_all);
        });
        this.composer
            .update(cx, |i, cx| i.set_value("typed after".into(), cx));
        assert!(this.save_draft(cx));
        settle(this, cx);
        assert_eq!(
            this.status_line(cx),
            None,
            "the folder warning does not return"
        );
    });
}

#[gpui::test]
fn send_with_unrelated_save_debt_refuses_without_blaming_a_folder(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open(cx, true);
    view.update_in(cx, |this, window, cx| {
        refuse_directory_sync(this, cx);
        this.set_approval_mode(1, ApprovalMode::ReadOnly, window, cx);
        this.composer
            .update(cx, |i, cx| i.set_value("Retry later".into(), cx));
        this.send_message(
            crate::ComposerDraft {
                session: 1,
                text: "Retry later".into(),
                attachments: Vec::new(),
            },
            window,
            cx,
        );
        assert!(this.state.read(cx).test_codex().is_none());
        assert_eq!(this.composer.read(cx).value(), "Retry later");
        assert!(this.state.read(cx).conversations()[&1].rows.is_empty());
        assert_eq!(
            this.state.read(cx).conversations()[&1].error.as_deref(),
            Some("Not sent. Could not finish saving changes. directory sync unavailable")
        );
    });
}

#[gpui::test]
fn first_send_lock_waits_for_the_send_barrier_when_its_sync_is_pending(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open(cx, true);
    view.update_in(cx, |this, window, cx| {
        assert!(!this.state.read(cx).store().workspace().sessions()[0].started());
        refuse_directory_sync(this, cx);
        this.composer
            .update(cx, |i, cx| i.set_value("First message".into(), cx));
        this.send_message(
            crate::ComposerDraft {
                session: 1,
                text: "First message".into(),
                attachments: Vec::new(),
            },
            window,
            cx,
        );
        assert!(this.state.read(cx).test_codex().is_none());
        assert_eq!(this.composer.read(cx).value(), "First message");
        assert!(
            this.state.read(cx).conversations()[&1]
                .error
                .as_deref()
                .is_some_and(|e| e.starts_with("Not sent. Could not finish saving changes.")),
            "{:?}",
            this.state.read(cx).conversations()[&1].error
        );
        let saved = this
            .state
            .read(cx)
            .store()
            .saved_snapshot()
            .unwrap()
            .to_string();
        assert!(saved.contains(r#""model_locked":true"#), "{saved}");
        assert!(this.state.read(cx).store().workspace().sessions()[0].started());
    });
}

fn fake_codex() -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap();
    exe.parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("keystone-fake-codex")
}

/// The scripted Claude itself, run straight from the build, so the test writes no script.
fn fake_claude() -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap();
    let fake = exe
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("keystone-fake-claude");
    assert!(
        fake.exists(),
        "build it with `cargo build -p keystone-agent --bins`"
    );
    fake
}

fn logged_claude(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let fake = fake_claude();
    let log = dir.join("claude-cwd");
    let script = dir.join("claude-cwd-wrapper");
    keystone_core::test_support::write_executable(
        &script,
        format!(
            "#!/bin/sh\nif [ \"$1\" != --version ]; then pwd >> '{}'; fi\nexec '{}' \"$@\"\n",
            log.display(),
            fake.display()
        ),
    );
    (script, log)
}

fn claude_from(this: &mut Keystone, program: std::path::PathBuf, cx: &mut Context<Keystone>) {
    this.state.update(cx, |state, _| {
        state.test_set_claude(Some(keystone_agent::ClaudeConnection::new(
            Some(program),
            state.test_wake().clone(),
        )));
    });
}

/// The model names the menu would list for the session once its catalog arrives.
fn listed(
    view: &Entity<Keystone>,
    cx: &mut VisualTestContext,
    session: u64,
    provider: Provider,
) -> Vec<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        cx.run_until_parked();
        let ready = view.read_with(cx, |this, cx| {
            match this.model_selection(session, provider, cx).choices {
                Choices::Ready(models) => Some(models.iter().map(|m| m.model.clone()).collect()),
                _ => None,
            }
        });
        if let Some(models) = ready {
            return models;
        }
        assert!(std::time::Instant::now() < deadline, "no catalog arrived");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn started_in(log: &Path) -> Vec<std::path::PathBuf> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(std::path::PathBuf::from)
        .collect()
}

fn canonical(path: std::path::PathBuf) -> std::path::PathBuf {
    path.canonicalize().unwrap()
}

#[gpui::test]
fn changing_provider_clears_a_completed_catalog_notice(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(cx, true);
    let (claude, _) = logged_claude(dir.path());
    let body = std::fs::read_to_string(&claude).unwrap().replace(
        "#!/bin/sh",
        "#!/bin/sh\nexport KEYSTONE_FAKE_CLAUDE_VERSION=development-build",
    );
    keystone_core::test_support::write_executable(&claude, body);
    view.update(cx, |this, cx| {
        claude_from(this, claude, cx);
        this.request_catalog(2, Provider::Claude, cx);
    });
    listed(&view, cx, 2, Provider::Claude);
    view.update_in(cx, |this, window, cx| {
        this.state.update(cx, |state, _| {
            state.test_conversation(2).restore(Ok(Vec::new()))
        });
        assert!(
            this.state.read(cx).conversations()[&2]
                .provider_notice
                .as_deref()
                .unwrap()
                .contains("Claude Code")
        );
        this.choose_provider(2, Provider::Codex, window, cx);
        assert!(
            this.state.read(cx).conversations()[&2]
                .provider_notice
                .is_none()
        );
    });
}

#[gpui::test]
fn a_late_catalog_notice_cannot_cross_provider_selection(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(cx, true);
    let (claude, _) = logged_claude(dir.path());
    let body = std::fs::read_to_string(&claude).unwrap().replace(
        "#!/bin/sh",
        "#!/bin/sh\nexport KEYSTONE_FAKE_CLAUDE_VERSION=development-build",
    );
    keystone_core::test_support::write_executable(&claude, body);
    // The scripted Codex, since a Codex installed on the machine may earn a notice of its own.
    view.update_in(cx, |this, window, cx| {
        claude_from(this, claude, cx);
        this.state.update(cx, |state, _| {
            state.test_set_codex(Some(keystone_agent::CodexConnection::new(
                Some(fake_codex()),
                "test",
                state.test_wake().clone(),
            )));
        });
        this.request_catalog(2, Provider::Claude, cx);
        this.choose_provider(2, Provider::Codex, window, cx);
    });
    listed(&view, cx, 2, Provider::Claude);
    view.update_in(cx, |this, window, cx| {
        let _ = window;
        this.pump(cx);
        this.state.update(cx, |state, _| {
            state.test_conversation(2).restore(Ok(Vec::new()))
        });
        assert!(
            this.state.read(cx).conversations()[&2]
                .provider_notice
                .is_none()
        );
    });
}

#[gpui::test]
fn a_draft_catalog_starts_in_the_conversation_folder_not_the_layout_project(
    cx: &mut TestAppContext,
) {
    let (dir, view, cx) = open(cx, true);
    pick(&view, cx, &dir.path().join("b"));
    let (claude, log) = logged_claude(dir.path());
    view.update(cx, |this, cx| {
        claude_from(this, claude, cx);
        this.request_catalog(2, Provider::Claude, cx);
    });
    assert_eq!(
        listed(&view, cx, 2, Provider::Claude),
        ["fake-opus", "fake-haiku"]
    );
    assert_eq!(started_in(&log), [canonical(dir.path().join("a"))]);
}

#[gpui::test]
fn a_draft_without_a_usable_folder_asks_nothing_and_leaves_other_drafts_their_catalog(
    cx: &mut TestAppContext,
) {
    let (dir, view, cx) = open(cx, false);
    let (claude, log) = logged_claude(dir.path());
    view.update(cx, |this, cx| {
        let a = WorkingDirectory::new(&dir.path().join("a")).unwrap();
        assert_eq!(
            this.state
                .update(cx, |state, cx| state.create(Provider::Claude, &a, 3, cx))
                .unwrap()
                .value,
            3
        );
        claude_from(this, claude, cx);
        for (session, provider) in [(1, Provider::Codex), (2, Provider::Claude)] {
            this.request_catalog(session, provider, cx);
            assert!(matches!(
                this.model_selection(session, provider, cx).choices,
                Choices::Unavailable(_)
            ));
        }
        assert!(this.state.read(cx).test_codex().is_none());
        this.request_catalog(3, Provider::Claude, cx);
    });
    assert_eq!(
        listed(&view, cx, 3, Provider::Claude),
        ["fake-opus", "fake-haiku"]
    );
    assert_eq!(started_in(&log), [canonical(dir.path().join("a"))]);
    view.read_with(cx, |this, cx| {
        assert!(matches!(
            this.model_selection(2, Provider::Claude, cx).choices,
            Choices::Unavailable(_)
        ));
    });
}

#[gpui::test]
fn a_chosen_legacy_folder_loads_its_catalog_without_a_retry(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(cx, false);
    let (claude, log) = logged_claude(dir.path());
    view.update(cx, |this, cx| {
        this.state
            .update(cx, |state, cx| state.set_provider(1, Provider::Claude, cx))
            .unwrap();
        claude_from(this, claude, cx);
        this.request_catalog(1, Provider::Claude, cx);
        assert!(matches!(
            this.model_selection(1, Provider::Claude, cx).choices,
            Choices::Unavailable(_)
        ));
        let ticket = this.presentation.legacy_ticket(1).unwrap();
        this.finish_legacy_folder(ticket, FolderPick::Selected(dir.path().join("b")), cx);
        this.request_catalog(1, Provider::Claude, cx);
    });
    assert_eq!(
        listed(&view, cx, 1, Provider::Claude),
        ["fake-opus", "fake-haiku"]
    );
    assert_eq!(started_in(&log), [canonical(dir.path().join("b"))]);
}

#[gpui::test]
fn drafts_in_two_folders_each_get_the_catalog_of_their_own_folder(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(cx, true);
    let only_b = serde_json::json!([{"type": "control_response", "response": {
    "subtype": "success", "request_id": "keystone-catalog", "response": {"models": [
        {"value": "b-only", "displayName": "B only", "description": "", "resolvedModel": "b-only"}
    ]}}}]);
    std::fs::write(
        dir.path().join("b/.fake-claude-initialize-frames"),
        only_b.to_string(),
    )
    .unwrap();
    let (claude, log) = logged_claude(dir.path());
    view.update(cx, |this, cx| {
        let b = WorkingDirectory::new(&dir.path().join("b")).unwrap();
        assert_eq!(
            this.state
                .update(cx, |state, cx| state.create(Provider::Claude, &b, 3, cx))
                .unwrap()
                .value,
            3
        );
        claude_from(this, claude, cx);
        this.request_catalog(2, Provider::Claude, cx);
        this.request_catalog(3, Provider::Claude, cx);
    });
    assert_eq!(listed(&view, cx, 3, Provider::Claude), ["b-only"]);
    assert_eq!(
        listed(&view, cx, 2, Provider::Claude),
        ["fake-opus", "fake-haiku"]
    );
    let mut started = started_in(&log);
    started.sort();
    assert_eq!(
        started,
        [
            canonical(dir.path().join("a")),
            canonical(dir.path().join("b"))
        ]
    );
}

fn apply(view: &Entity<Keystone>, cx: &mut VisualTestContext, session: u64, event: Event) {
    view.update(cx, |this, cx| {
        this.receive(Vec::new(), vec![(session, event)], cx);
        cx.notify();
    });
    cx.run_until_parked();
}

fn click(cx: &mut VisualTestContext, id: &'static str) {
    let bounds = cx
        .debug_bounds(id)
        .unwrap_or_else(|| panic!("missing {id}"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
}

fn inside(inner: gpui::Bounds<gpui::Pixels>, outer: gpui::Bounds<gpui::Pixels>) -> bool {
    inner.left() >= outer.left()
        && inner.right() <= outer.right()
        && inner.top() >= outer.top()
        && inner.bottom() <= outer.bottom()
}

#[gpui::test]
fn a_400px_composer_keeps_a_model_target_and_every_action_through_a_turn(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open(cx, true);
    cx.simulate_resize(gpui::size(px(400.), px(800.)));
    let window = gpui::Bounds::new(gpui::point(px(0.), px(0.)), gpui::size(px(400.), px(800.)));
    for session in [1, 2] {
        view.update(cx, |this, cx| {
            this.select(session, cx);
            // Rendering would start the history load only after the flush, and events
            // that arrive while it loads wait for it, Stop included.
            this.state.update(cx, |state, cx| {
                state.ensure_history(session);
                state.flush_logs(cx).unwrap()
            });
        });
        cx.run_until_parked();
        let check = |cx: &mut VisualTestContext, actions: &[&'static str]| {
            let selector = cx.debug_bounds("model-selector").unwrap();
            let folder = cx.debug_bounds("project").unwrap();
            let approval = cx.debug_bounds("approval-mode").unwrap();
            let send = cx.debug_bounds("send").unwrap();
            let field = cx.debug_bounds("composer-field").unwrap();
            assert!(
                selector.size.width >= px(56.) && selector.size.height >= px(24.),
                "{selector:?}"
            );
            assert!(selector.right() <= folder.left() && folder.right() <= approval.left());
            assert_eq!(folder.size.width, px(24.));
            assert!(send.right() <= field.right() + px(12.), "Send at {send:?}");
            for &id in actions {
                let action = cx
                    .debug_bounds(id)
                    .unwrap_or_else(|| panic!("{id} missing"));
                assert!(
                    approval.right() <= action.left() && action.right() <= send.left(),
                    "{id} at {action:?}"
                );
                assert!(inside(action, window));
            }
            assert!(inside(send, window));
        };
        check(cx, &[]);
        view.update(cx, |this, cx| {
            this.state
                .update(cx, |state, cx| state.lock_model(session, cx))
                .unwrap()
        });
        apply(&view, cx, session, Event::TurnStarted);
        check(cx, &["stop"]);
        apply(
            &view,
            cx,
            session,
            Event::ApprovalRequested(keystone_agent::Approval {
                request: keystone_agent::RequestId("1".into()),
                item_id: None,
                action: "cargo test".into(),
                detail: None,
            }),
        );
        check(cx, &["stop"]);
        assert!(cx.debug_bounds("approve").is_some());
    }
}

#[gpui::test]
fn the_model_menu_fits_a_400px_window_and_keeps_its_columns_when_wide(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(cx, true);
    let fail = |view: &Entity<Keystone>, cx: &mut VisualTestContext| {
        view.update(cx, |this, cx| {
            let key = CatalogKey {
                provider: Provider::Codex,
                account: None,
                directory: WorkingDirectory::new(&dir.path().join("a")).unwrap(),
            };
            this.state
                .update(cx, |state, _cx| state.test_catalogs_mut().clear(&key));
            this.state
                .update(cx, |state, _cx| state.test_catalogs_mut().ask(key, 1));
            this.state.update(cx, |state, _cx| {
                state.test_catalogs_mut().apply(
                    Provider::Codex,
                    None,
                    1,
                    keystone_agent::Catalog::Failed(
                        "Codex stopped before it listed its models, with a long reason.".into(),
                    ),
                )
            });
            this.model_menu = Some(1);
            cx.notify();
        });
        cx.run_until_parked();
    };
    cx.simulate_resize(gpui::size(px(1280.), px(800.)));
    fail(&view, cx);
    let provider = cx.debug_bounds("provider-codex").unwrap();
    let column = cx.debug_bounds("model-column").unwrap();
    assert_eq!(provider.size.width, px(152.));
    assert_eq!(column.size.width, px(282.));

    cx.simulate_resize(gpui::size(px(400.), px(800.)));
    fail(&view, cx);
    let window = gpui::Bounds::new(gpui::point(px(8.), px(8.)), gpui::size(px(384.), px(784.)));
    assert_eq!(
        cx.debug_bounds("provider-codex").unwrap().size.width,
        px(152.)
    );
    let column = cx.debug_bounds("model-column").unwrap();
    assert!(
        column.size.width < px(282.) && inside(column, window),
        "{column:?}"
    );
    for id in [
        "provider-claude",
        "provider-codex",
        "model-default",
        "model-failed",
        "model-retry",
    ] {
        let bounds = cx
            .debug_bounds(id)
            .unwrap_or_else(|| panic!("{id} missing"));
        assert!(inside(bounds, window), "{id} at {bounds:?}");
        assert!(
            id.starts_with("provider") || inside(bounds, column),
            "{id} at {bounds:?}"
        );
    }
}

/// The narrowest window width the default layout shows unfitted.
fn fitted_boundary(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> f32 {
    let fitted = |width: f32, cx: &mut VisualTestContext| {
        cx.simulate_resize(gpui::size(px(width), px(800.)));
        cx.run_until_parked();
        view.read_with(cx, |this, _cx| this.presentation.current.arrangement.fitted)
    };
    let (mut fits, mut unfit) = (2400., 400.);
    assert!(fitted(unfit, cx) && !fitted(fits, cx));
    while fits - unfit > 1. {
        let middle = ((fits + unfit) / 2_f32).floor();
        if fitted(middle, cx) {
            unfit = middle;
        } else {
            fits = middle;
        }
    }
    fits
}

#[gpui::test]
fn long_model_labels_and_reports_never_refit_the_layout(cx: &mut TestAppContext) {
    const LONG: &str =
        "fake-opus-with-a-provider-reported-identifier-longer-than-any-label-20991231";
    let (dir, view, cx) = open(cx, true);
    let (claude, _log) = logged_claude(dir.path());
    view.update(cx, |this, cx| {
        claude_from(this, claude, cx);
        this.state
            .update(cx, |state, cx| {
                state.set_choice(
                    2,
                    keystone_core::ModelChoice {
                        model: Some(LONG.into()),
                        ..Default::default()
                    },
                    cx,
                )
            })
            .unwrap();
        this.select(2, cx);
    });
    cx.run_until_parked();
    let boundary = fitted_boundary(&view, cx);
    let at_boundary = |cx: &mut VisualTestContext, step: &str| {
        assert_eq!(fitted_boundary(&view, cx), boundary, "refit after {step}");
        let selector = cx.debug_bounds("model-selector").unwrap();
        let folder = cx.debug_bounds("project").unwrap();
        assert!(selector.right() <= folder.left(), "{step}: {selector:?}");
    };

    view.update(cx, |this, cx| this.select(1, cx));
    cx.run_until_parked();
    at_boundary(cx, "selecting a Codex draft");
    view.update(cx, |this, cx| this.select(2, cx));
    cx.run_until_parked();
    view.update(cx, |this, cx| {
        let key = CatalogKey {
            provider: Provider::Claude,
            account: None,
            directory: WorkingDirectory::new(&dir.path().join("a")).unwrap(),
        };
        this.state
            .update(cx, |state, _cx| state.test_catalogs_mut().ask(key, 2));
        this.state.update(cx, |state, _cx| {
            state.test_catalogs_mut().apply(
                Provider::Claude,
                None,
                2,
                keystone_agent::Catalog::Ready {
                    models: vec![keystone_agent::ModelInfo {
                        model: LONG.into(),
                        label: "A catalog label far longer than the provider default label".into(),
                        resolved: None,
                        ..Default::default()
                    }],
                    default_model: None,
                },
            )
        });
    });
    at_boundary(cx, "a long catalog label");

    cx.simulate_resize(gpui::size(px(boundary), px(800.)));
    view.update_in(cx, |this, window, cx| {
        this.composer
            .update(cx, |input, cx| input.set_value("hello".into(), cx));
        this.send_message(
            crate::ComposerDraft {
                session: 2,
                text: "hello".into(),
                attachments: Vec::new(),
            },
            window,
            cx,
        );
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while view.read_with(cx, |this, cx| {
        this.state.read(cx).conversations()[&2]
            .reported_model
            .as_deref()
            != Some(LONG)
    }) {
        assert!(std::time::Instant::now() < deadline, "no model report");
        std::thread::sleep(std::time::Duration::from_millis(20));
        cx.run_until_parked();
        assert!(
            !view.read_with(cx, |this, _cx| this.presentation.current.arrangement.fitted),
            "the Send or its report refit the layout"
        );
    }
    at_boundary(cx, "the provider's report");
}

/// A catalog that lands while the menu is open moves no provider row: a press on
/// Claude Code that the catalog's arrival splits still chooses Claude Code.
#[gpui::test]
fn a_catalog_arriving_mid_click_keeps_the_provider_row_under_the_pointer(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(cx, true);
    cx.simulate_resize(gpui::size(px(1280.), px(900.)));
    let key = |provider| CatalogKey {
        provider,
        account: None,
        directory: WorkingDirectory::new(&dir.path().join("a")).unwrap(),
    };
    view.update(cx, |this, cx| {
        this.state.update(cx, |state, _cx| {
            state.test_catalogs_mut().ask(key(Provider::Codex), 1);
            state.test_catalogs_mut().ask(key(Provider::Claude), 1);
        });
        this.model_menu = Some(1);
        cx.notify();
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("model-loading").is_some());
    let claude = cx.debug_bounds("provider-claude").unwrap();
    cx.simulate_event(gpui::MouseDownEvent {
        position: claude.center(),
        modifiers: gpui::Modifiers::none(),
        button: gpui::MouseButton::Left,
        click_count: 1,
        first_mouse: false,
    });
    view.update(cx, |this, cx| {
        this.state.update(cx, |state, _cx| {
            state.test_catalogs_mut().apply(
                Provider::Codex,
                None,
                1,
                keystone_agent::Catalog::Ready {
                    models: ["one", "two", "three", "four", "five"]
                        .map(|model| keystone_agent::ModelInfo {
                            model: model.into(),
                            label: model.into(),
                            resolved: None,
                            ..Default::default()
                        })
                        .into(),
                    default_model: None,
                },
            )
        });
        cx.notify();
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("model-five").is_some());
    assert_eq!(cx.debug_bounds("provider-claude"), Some(claude));
    cx.simulate_event(gpui::MouseUpEvent {
        position: claude.center(),
        modifiers: gpui::Modifiers::none(),
        button: gpui::MouseButton::Left,
        click_count: 1,
    });
    assert_eq!(
        view.read_with(cx, |this, cx| this.provider(1, cx)),
        Some(Provider::Claude)
    );
    assert!(cx.debug_bounds("model-loading").is_some());
    assert_eq!(cx.debug_bounds("provider-claude"), Some(claude));
}

/// A model chosen below the fold is in view when the menu opens. A wheel
/// back to the top afterwards stays there: the reveal happens once per opening.
#[gpui::test]
fn the_model_menu_opens_on_a_chosen_model_below_the_fold(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(cx, true);
    cx.simulate_resize(gpui::size(px(1280.), px(900.)));
    let key = |provider| CatalogKey {
        provider,
        account: None,
        directory: WorkingDirectory::new(&dir.path().join("a")).unwrap(),
    };
    view.update(cx, |this, cx| {
        this.state.update(cx, |state, cx| {
            let catalogs = state.test_catalogs_mut();
            catalogs.ask(key(Provider::Codex), 1);
            catalogs.apply(
                Provider::Codex,
                None,
                1,
                keystone_agent::Catalog::Ready {
                    models: (0..16)
                        .map(|n| keystone_agent::ModelInfo {
                            model: format!("m{n}"),
                            label: format!("Model {n}"),
                            resolved: None,
                            ..Default::default()
                        })
                        .collect(),
                    default_model: None,
                },
            );
            state
                .set_choice(
                    1,
                    keystone_core::ModelChoice {
                        model: Some("m12".into()),
                        ..Default::default()
                    },
                    cx,
                )
                .unwrap();
        });
    });
    cx.run_until_parked();
    click(cx, "model-selector");
    cx.run_until_parked();
    let list = cx.debug_bounds("model-list").unwrap();
    let chosen = cx.debug_bounds("model-m12").unwrap();
    assert!(inside(chosen, list), "{chosen:?} outside {list:?}");
    assert!(
        cx.debug_bounds("model-m0")
            .is_none_or(|first| !inside(first, list)),
        "the list still starts at the top"
    );

    cx.simulate_event(gpui::ScrollWheelEvent {
        position: chosen.center(),
        delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(10000.))),
        ..Default::default()
    });
    cx.run_until_parked();
    view.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    let first = cx.debug_bounds("model-m0").unwrap();
    assert!(
        inside(first, list),
        "a re-render scrolled the list back to the choice"
    );

    click(cx, "model-selector");
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |this, _cx| this.model_menu), None);
    click(cx, "model-selector");
    cx.run_until_parked();
    let chosen = cx.debug_bounds("model-m12").unwrap();
    assert!(
        inside(chosen, list),
        "reopened: {chosen:?} outside {list:?}"
    );
}

/// An error line that arrives or clears on its own while a composer menu is open moves
/// neither menu nor its button, and a press it splits still lands on the row pressed.
#[gpui::test]
fn an_error_arriving_mid_click_keeps_the_menus_under_the_pointer(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(cx, true);
    cx.simulate_resize(gpui::size(px(1280.), px(900.)));
    let directory = WorkingDirectory::new(&dir.path().join("a")).unwrap();
    view.update(cx, |this, cx| {
        this.state.update(cx, |state, _cx| {
            let catalogs = state.test_catalogs_mut();
            catalogs.ask(
                CatalogKey {
                    provider: Provider::Codex,
                    account: None,
                    directory: directory.clone(),
                },
                1,
            );
            catalogs.apply(
                Provider::Codex,
                None,
                1,
                keystone_agent::Catalog::Ready {
                    models: ["one", "two", "three"]
                        .map(|model| keystone_agent::ModelInfo {
                            model: model.into(),
                            label: model.into(),
                            resolved: None,
                            ..Default::default()
                        })
                        .into(),
                    default_model: None,
                },
            )
        });
    });
    let set_error = |error: Option<&str>, cx: &mut VisualTestContext| {
        view.update(cx, |this, cx| {
            this.state.update(cx, |state, cx| {
                state.error = error.map(Into::into);
                cx.notify();
            })
        });
        cx.run_until_parked();
    };
    let press = |position, cx: &mut VisualTestContext| {
        cx.simulate_mouse_down(position, gpui::MouseButton::Left, gpui::Modifiers::none())
    };
    let release = |position, cx: &mut VisualTestContext| {
        cx.simulate_mouse_up(position, gpui::MouseButton::Left, gpui::Modifiers::none())
    };
    let error = "Could not save the session. database is locked";

    click(cx, "model-selector");
    cx.run_until_parked();
    let selector = cx.debug_bounds("model-selector").unwrap();
    let three = cx.debug_bounds("model-three").unwrap();
    press(three.center(), cx);
    set_error(Some(error), cx);
    assert!(cx.debug_bounds("workspace-error").is_some());
    assert_eq!(cx.debug_bounds("model-three"), Some(three));
    assert_eq!(cx.debug_bounds("model-selector"), Some(selector));
    release(three.center(), cx);
    cx.run_until_parked();
    assert_eq!(
        view.read_with(cx, |this, cx| this
            .selected_session(cx)
            .unwrap()
            .model()
            .map(str::to_owned)),
        Some("three".into())
    );

    click(cx, "model-selector");
    cx.run_until_parked();
    let selector = cx.debug_bounds("model-selector").unwrap();
    let two = cx.debug_bounds("model-two").unwrap();
    press(two.center(), cx);
    set_error(None, cx);
    assert!(cx.debug_bounds("workspace-error").is_none());
    assert_eq!(cx.debug_bounds("model-two"), Some(two));
    assert_eq!(cx.debug_bounds("model-selector"), Some(selector));
    release(two.center(), cx);
    cx.run_until_parked();
    assert_eq!(
        view.read_with(cx, |this, cx| this
            .selected_session(cx)
            .unwrap()
            .model()
            .map(str::to_owned)),
        Some("two".into())
    );

    let current = view.read_with(cx, |this, cx| {
        this.selected_session(cx).unwrap().approval_mode()
    });
    let mode = ApprovalMode::ALL
        .into_iter()
        .find(|mode| *mode != current && *mode != ApprovalMode::FullAccess)
        .unwrap();
    let trigger = cx.debug_bounds("approval-mode").unwrap();
    click(cx, "approval-mode");
    cx.run_until_parked();
    let selector = format!("approval-{}", mode.slug()).leak();
    let option = cx.debug_bounds(selector).unwrap();
    press(option.center(), cx);
    set_error(Some(error), cx);
    assert_eq!(cx.debug_bounds(selector), Some(option));
    assert_eq!(cx.debug_bounds("approval-mode"), Some(trigger));
    release(option.center(), cx);
    cx.run_until_parked();
    assert_eq!(
        view.read_with(cx, |this, cx| this
            .selected_session(cx)
            .unwrap()
            .approval_mode()),
        mode
    );
}

/// The model choices keep one height through every catalog state and both providers,
/// in a short window and at Spacious density too. A failure message of any length
/// scrolls inside them and leaves Retry in view, and the whole menu stays in the window.
#[gpui::test]
fn the_model_choices_keep_one_height_and_retry_stays_in_view(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(cx, true);
    let key = |provider| CatalogKey {
        provider,
        account: None,
        directory: WorkingDirectory::new(&dir.path().join("a")).unwrap(),
    };
    let show = |catalog: Option<keystone_agent::Catalog>, cx: &mut VisualTestContext| {
        view.update(cx, |this, cx| {
            this.state.update(cx, |state, _cx| {
                let catalogs = state.test_catalogs_mut();
                catalogs.clear(&key(Provider::Codex));
                catalogs.ask(key(Provider::Codex), 1);
                catalogs.ask(key(Provider::Claude), 1);
                if let Some(catalog) = catalog {
                    catalogs.apply(Provider::Codex, None, 1, catalog);
                }
            });
            this.model_menu = Some(1);
            cx.notify();
        });
        cx.run_until_parked();
        cx.debug_bounds("model-column").unwrap()
    };
    let long = "Codex stopped before it listed its models: ".to_owned()
        + &"a line of stderr that keeps going ".repeat(120);
    let models: Vec<_> = (0..12)
        .map(|n| keystone_agent::ModelInfo {
            model: format!("model-{n}"),
            label: format!("Model {n}"),
            resolved: None,
            ..Default::default()
        })
        .collect();
    for (width, height, spacious) in [
        (1280., 900., false),
        (400., 800., false),
        (1280., 360., false),
        (1280., 360., true),
    ] {
        view.update_in(cx, |this, window, cx| {
            this.update_appearance(
                |appearance| {
                    appearance.density = if spacious {
                        keystone_core::Density::Spacious
                    } else {
                        keystone_core::Density::Default
                    }
                },
                window,
                cx,
            );
            this.state
                .update(cx, |state, cx| state.set_provider(1, Provider::Codex, cx))
                .unwrap();
        });
        cx.simulate_resize(gpui::size(px(width), px(height)));
        let at = format!("{width}x{height} spacious {spacious}");
        let window = gpui::Bounds::new(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(width), px(height)),
        );
        let loading = show(None, cx);
        assert!(inside(loading, window), "{at}: {loading:?}");
        let failed = show(Some(keystone_agent::Catalog::Failed(long.clone())), cx);
        assert_eq!(failed, loading, "{at}: a long failure");
        let retry = cx.debug_bounds("model-retry").unwrap();
        assert!(inside(retry, loading), "{at}: Retry at {retry:?}");
        let ready = show(
            Some(keystone_agent::Catalog::Ready {
                models: models.clone(),
                default_model: None,
            }),
            cx,
        );
        assert_eq!(ready, loading, "{at}: a long catalog");
        let claude = cx.debug_bounds("provider-claude").unwrap();
        cx.simulate_click(claude.center(), gpui::Modifiers::none());
        assert_eq!(
            view.read_with(cx, |this, cx| this.provider(1, cx)),
            Some(Provider::Claude),
            "{at}"
        );
        assert_eq!(
            cx.debug_bounds("model-column"),
            Some(loading),
            "{at}: Claude"
        );
    }
}

/// Many accounts fit a short window too: their list scrolls inside the menu instead of
/// pushing the menu past the window's edge, and the last account scrolls into view.
#[gpui::test]
fn many_accounts_scroll_inside_a_short_window(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(cx, true);
    let last = view.update(cx, |this, cx| {
        this.state.update(cx, |state, cx| {
            (0..7)
                .map(|n| {
                    let folder = dir.path().join(format!("accounts/codex-{n}"));
                    state
                        .add_account(
                            Provider::Codex,
                            &format!("Account {n}"),
                            folder.to_str().unwrap(),
                            cx,
                        )
                        .unwrap()
                        .value
                })
                .last()
                .unwrap()
        })
    });
    for spacious in [false, true] {
        view.update_in(cx, |this, window, cx| {
            this.update_appearance(
                |appearance| {
                    appearance.density = if spacious {
                        keystone_core::Density::Spacious
                    } else {
                        keystone_core::Density::Default
                    }
                },
                window,
                cx,
            );
            this.model_menu = Some(1);
            cx.notify();
        });
        cx.simulate_resize(gpui::size(px(1280.), px(360.)));
        cx.run_until_parked();
        let window =
            gpui::Bounds::new(gpui::point(px(0.), px(0.)), gpui::size(px(1280.), px(360.)));
        let list = cx.debug_bounds("provider-list").unwrap();
        let column = cx.debug_bounds("model-column").unwrap();
        assert!(inside(list, window), "spacious {spacious}: {list:?}");
        assert!(inside(column, window), "spacious {spacious}: {column:?}");
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: list.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-10000.))),
            ..Default::default()
        });
        cx.run_until_parked();
        let account = cx.debug_bounds(format!("account-{last}").leak()).unwrap();
        assert!(
            inside(account, list),
            "spacious {spacious}: {account:?} in {list:?}"
        );
    }
}

#[gpui::test]
fn choosing_a_folder_outside_the_project_for_an_old_conversation_follows_it(
    cx: &mut TestAppContext,
) {
    let (dir, view, cx) = open(cx, false);
    let b = ProjectId::new(&dir.path().join("b")).unwrap();
    view.update(cx, |this, cx| {
        assert_eq!(this.selected(cx), Some(1));
        let ticket = this.presentation.legacy_ticket(1).unwrap();
        this.finish_legacy_folder(ticket, FolderPick::Selected(dir.path().join("b")), cx);
        assert_eq!(this.presentation.project.as_ref().ok(), Some(&b));
        assert_eq!(this.selected(cx), Some(1), "the conversation stays shown");
        assert_eq!(this.error, None);
    });
}

#[test]
fn a_model_is_named_by_its_catalog_label_and_an_unknown_default_stays_default() {
    let models = [keystone_agent::ModelInfo {
        model: "opus".into(),
        label: "Opus 5.5".into(),
        resolved: Some("claude-opus-5-5".into()),
        ..Default::default()
    }];
    let named =
        |model, default, choices| crate::model_selector::requested_model(model, default, choices);
    assert_eq!(
        named(Some("opus"), None, Choices::Ready(&models)),
        "Opus 5.5"
    );
    assert_eq!(
        named(None, Some("claude-opus-5-5"), Choices::Ready(&models)),
        "Opus 5.5",
        "the default, by what the provider says it resolves to"
    );
    assert_eq!(named(None, None, Choices::Ready(&models)), "Default");
    assert_eq!(
        named(None, Some("claude-opus-5-5"), Choices::Loading),
        "Default",
        "never a guess"
    );
}

#[gpui::test]
fn a_default_claude_session_names_its_model_before_the_first_message(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open(cx, true);
    view.update(cx, |this, cx| {
        claude_from(this, fake_claude(), cx);
        this.select(2, cx);
    });
    let named = |session, provider, cx: &mut VisualTestContext| {
        view.read_with(cx, |this, cx| {
            let selection = this.state.read(cx).model_selection(session, provider);
            crate::model_selector::requested_model(
                selection.model,
                selection.default_model,
                selection.choices,
            )
        })
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while named(2, Provider::Claude, cx) != "Fake Opus" {
        assert!(
            std::time::Instant::now() < deadline,
            "the chip never named the default model: {}",
            named(2, Provider::Claude, cx)
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
        cx.run_until_parked();
    }
    assert_eq!(
        named(1, Provider::Codex, cx),
        "Default",
        "Codex does not say what its default runs as"
    );
}

#[gpui::test]
fn the_model_a_turn_reports_is_named_again_after_a_relaunch(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open(cx, true);
    view.update(cx, |this, cx| {
        claude_from(this, fake_claude(), cx);
        this.select(2, cx);
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let until = |cx: &mut VisualTestContext, what: &str, done: &dyn Fn(&Keystone, &App) -> bool| {
        while !view.read_with(cx, |this, cx| done(this, cx)) {
            assert!(std::time::Instant::now() < deadline, "{what}");
            std::thread::sleep(std::time::Duration::from_millis(20));
            cx.run_until_parked();
        }
    };
    until(cx, "the catalog loads", &|this, cx| {
        let selection = this.state.read(cx).model_selection(2, Provider::Claude);
        crate::model_selector::requested_model(
            selection.model,
            selection.default_model,
            selection.choices,
        ) == "Fake Opus"
    });
    view.update_in(cx, |this, window, cx| {
        this.send_message(
            crate::ComposerDraft {
                session: 2,
                text: "hello".into(),
                attachments: Vec::new(),
            },
            window,
            cx,
        );
    });
    until(cx, "the turn's model is saved", &|this, cx| {
        this.state
            .read(cx)
            .session(2)
            .and_then(|s| s.last_model_name())
            == Some("Fake Opus")
    });
    // A relaunch keeps the saved session but not the provider's report from this run.
    view.update(cx, |this, cx| {
        this.state.update(cx, |state, _| {
            state
                .test_conversation(2)
                .apply(keystone_agent::Event::ModelLost)
        });
    });
    let (reported, shown) = view.read_with(cx, |this, cx| {
        let selection = this.state.read(cx).model_selection(2, Provider::Claude);
        (
            selection.reported_model.map(str::to_owned),
            selection.last_model_name.map(str::to_owned),
        )
    });
    assert_eq!(shown.as_deref(), Some("Fake Opus"));
    assert_eq!(
        reported, None,
        "effort and speed still follow only what the provider reports in this run"
    );
}
