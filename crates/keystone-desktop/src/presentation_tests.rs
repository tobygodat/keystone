use super::*;
use crate::FOLDER_UNCONFIRMED;
use crate::arrangement_editor::{Action, Commit};
use crate::customization_agent::{Outcome, Proposal};
use crate::model_selector::{CatalogKey, Choices};
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

#[gpui::test]
fn first_send_lock_waits_for_the_send_barrier_when_its_sync_is_pending(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open(cx, true);
    view.update_in(cx, |this, window, cx| {
        assert!(!this.store.workspace().sessions()[0].started());
        refuse_directory_sync(this);
        this.composer
            .update(cx, |i, cx| i.set_value("First message".into(), cx));
        this.send_message(
            crate::ComposerDraft {
                session: 1,
                text: "First message".into(),
            },
            window,
            cx,
        );
        assert!(this.codex.is_none());
        assert_eq!(this.composer.read(cx).value(), "First message");
        assert!(
            this.conversation(1)
                .error
                .as_deref()
                .is_some_and(|e| e.starts_with("Not sent. Could not finish saving changes.")),
            "{:?}",
            this.conversation(1).error
        );
        let saved = std::fs::read_to_string(this.store.path()).unwrap();
        assert!(saved.contains(r#""model_locked":true"#), "{saved}");
        assert!(this.store.workspace().sessions()[0].started());
    });
}

fn logged_claude(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
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
    let log = dir.join("claude-cwd");
    let script = dir.join("claude-cwd-wrapper");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nif [ \"$1\" != --version ]; then pwd >> '{}'; fi\nexec '{}' \"$@\"\n",
            log.display(),
            fake.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (script, log)
}

fn claude_from(this: &mut Keystone, program: std::path::PathBuf) {
    this.claude = Some(keystone_agent::ClaudeConnection::new(
        Some(program),
        this.wake.clone(),
    ));
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
        let ready = view.read_with(cx, |this, _| {
            match this.model_selection(session, provider).choices {
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
    std::fs::write(&claude, body).unwrap();
    view.update(cx, |this, cx| {
        claude_from(this, claude);
        this.request_catalog(2, Provider::Claude, cx);
    });
    listed(&view, cx, 2, Provider::Claude);
    view.update_in(cx, |this, window, cx| {
        this.conversation(2).restore(Ok(Vec::new()));
        assert!(
            this.conversation(2)
                .provider_notice
                .as_deref()
                .unwrap()
                .contains("Claude Code")
        );
        this.choose_provider(2, Provider::Codex, window, cx);
        assert!(this.conversation(2).provider_notice.is_none());
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
    std::fs::write(&claude, body).unwrap();
    view.update_in(cx, |this, window, cx| {
        claude_from(this, claude);
        this.request_catalog(2, Provider::Claude, cx);
        this.choose_provider(2, Provider::Codex, window, cx);
    });
    listed(&view, cx, 2, Provider::Claude);
    view.update_in(cx, |this, window, cx| {
        let _ = window;
        this.pump(cx);
        this.conversation(2).restore(Ok(Vec::new()));
        assert!(this.conversation(2).provider_notice.is_none());
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
        claude_from(this, claude);
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
        assert_eq!(this.store.create(Provider::Claude, &a, 3).unwrap().value, 3);
        claude_from(this, claude);
        for (session, provider) in [(1, Provider::Codex), (2, Provider::Claude)] {
            this.request_catalog(session, provider, cx);
            assert!(matches!(
                this.model_selection(session, provider).choices,
                Choices::Unavailable(_)
            ));
        }
        assert!(this.codex.is_none());
        this.request_catalog(3, Provider::Claude, cx);
    });
    assert_eq!(
        listed(&view, cx, 3, Provider::Claude),
        ["fake-opus", "fake-haiku"]
    );
    assert_eq!(started_in(&log), [canonical(dir.path().join("a"))]);
    view.read_with(cx, |this, _| {
        assert!(matches!(
            this.model_selection(2, Provider::Claude).choices,
            Choices::Unavailable(_)
        ));
    });
}

#[gpui::test]
fn a_chosen_legacy_folder_loads_its_catalog_without_a_retry(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(cx, false);
    let (claude, log) = logged_claude(dir.path());
    view.update(cx, |this, cx| {
        this.store.set_provider(1, Provider::Claude).unwrap();
        claude_from(this, claude);
        this.request_catalog(1, Provider::Claude, cx);
        assert!(matches!(
            this.model_selection(1, Provider::Claude).choices,
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
        assert_eq!(this.store.create(Provider::Claude, &b, 3).unwrap().value, 3);
        claude_from(this, claude);
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
        view.update(cx, |this, cx| this.select(session, cx));
        cx.run_until_parked();
        let check = |cx: &mut VisualTestContext, actions: &[&'static str]| {
            let selector = cx.debug_bounds("model-selector").unwrap();
            let folder = cx.debug_bounds("project").unwrap();
            let approval = cx.debug_bounds("approval-mode").unwrap();
            let send = cx.debug_bounds("send").unwrap();
            let field = cx.debug_bounds("composer-field").unwrap();
            assert!(
                selector.size.width >= px(56.) && selector.size.height >= px(28.),
                "{selector:?}"
            );
            assert!(selector.right() <= folder.left() && folder.right() <= approval.left());
            assert_eq!(folder.size.width, px(28.));
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
        view.update(cx, |this, _| this.store.lock_model(session).unwrap());
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
                directory: WorkingDirectory::new(&dir.path().join("a")).unwrap(),
            };
            this.catalogs.clear(&key);
            this.catalogs.ask(key, 1);
            this.catalogs.apply(
                Provider::Codex,
                1,
                keystone_agent::Catalog::Failed(
                    "Codex stopped before it listed its models, with a long reason.".into(),
                ),
            );
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
        "model-footer",
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
        view.read_with(cx, |this, _| this.presentation.current.arrangement.fitted)
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
        claude_from(this, claude);
        this.store.set_model(2, Some(LONG.into())).unwrap();
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
    view.update(cx, |this, _| {
        let key = CatalogKey {
            provider: Provider::Claude,
            directory: WorkingDirectory::new(&dir.path().join("a")).unwrap(),
        };
        this.catalogs.ask(key, 2);
        this.catalogs.apply(
            Provider::Claude,
            2,
            keystone_agent::Catalog::Ready {
                models: vec![keystone_agent::ModelInfo {
                    model: LONG.into(),
                    label: "A catalog label far longer than the provider default label".into(),
                    resolved: None,
                }],
                default_model: None,
            },
        );
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
            },
            window,
            cx,
        );
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while view.read_with(cx, |this, _| {
        this.conversations[&2].reported_model.as_deref() != Some(LONG)
    }) {
        assert!(std::time::Instant::now() < deadline, "no model report");
        std::thread::sleep(std::time::Duration::from_millis(20));
        cx.run_until_parked();
        assert!(
            !view.read_with(cx, |this, _| this.presentation.current.arrangement.fitted),
            "the Send or its report refit the layout"
        );
    }
    at_boundary(cx, "the provider's report");
}
