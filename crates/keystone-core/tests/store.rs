use keystone_core::WorkingDirectory;
use keystone_core::{Appearance, ApprovalMode, Density, Preferences, Provider, Store, ThemeMode};
use rusqlite::Connection;
use std::fs;
use tempfile::tempdir;

#[test]
fn sessions_and_drafts_survive_restart() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.json");
    let mut store = Store::open(&path).unwrap();
    assert!(store.workspace().sessions().is_empty());
    assert_eq!(store.workspace().selected(), None);

    let claude = store
        .create(Provider::Claude, &working_directory(), 1)
        .unwrap()
        .value;
    let codex = store
        .create(Provider::Codex, &working_directory(), 1)
        .unwrap()
        .value;
    assert_eq!((claude, codex), (1, 2));
    store.select(claude).unwrap();
    store.set_settled(codex, true).unwrap();
    store
        .set_draft(claude, "First paragraph 🥔\n\n第二段\n".into())
        .unwrap();
    drop(store);

    let mut restored = Store::open(&path).unwrap();
    assert_eq!(restored.workspace().selected(), Some(claude));
    let sessions = restored.workspace().sessions();
    assert_eq!(
        sessions
            .iter()
            .map(|session| session.id())
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(sessions[0].title(), "New Claude session");
    assert_eq!(sessions[0].provider(), Provider::Claude);
    assert!(!sessions[0].settled());
    assert_eq!(sessions[0].draft(), "First paragraph 🥔\n\n第二段\n");
    assert_eq!(sessions[1].title(), "New Codex session");
    assert_eq!(sessions[1].provider(), Provider::Codex);
    assert!(sessions[1].settled());
    assert_eq!(sessions[1].draft(), "");

    restored.set_settled(codex, false).unwrap();
    assert!(!Store::open(&path).unwrap().workspace().sessions()[1].settled());
    assert_eq!(
        restored
            .create(Provider::Claude, &working_directory(), 1)
            .unwrap()
            .value,
        3
    );
}

#[test]
fn invalid_commands_leave_memory_and_file_unchanged() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.json");
    let mut store = Store::open(&path).unwrap();
    store
        .create(Provider::Claude, &working_directory(), 1)
        .unwrap();
    let before = Store::open(&path).unwrap().saved_snapshot().unwrap();

    assert!(store.select(99).is_err());
    assert!(store.set_settled(99, true).is_err());
    assert!(store.set_draft(99, "lost".into()).is_err());
    assert_eq!(
        Store::open(&path).unwrap().saved_snapshot().unwrap(),
        before
    );
    assert_eq!(store.workspace().selected(), Some(1));
    assert!(!store.workspace().sessions()[0].settled());
    assert_eq!(store.workspace().sessions()[0].draft(), "");
}

#[test]
fn invalid_snapshots_are_not_overwritten() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.json");
    let cases = [
        "{",
        r#"{"version":2,"next_id":1,"workspace":{"sessions":[],"selected":null}}"#,
        r#"{"version":1,"next_id":1,"workspace":{"sessions":[],"selected":1}}"#,
        r#"{"version":1,"next_id":1,"workspace":{"sessions":[{"id":1,"title":"A","provider":"Claude","settled":false,"draft":""}],"selected":null}}"#,
        r#"{"version":1,"next_id":3,"workspace":{"sessions":[{"id":1,"title":"A","provider":"Claude","settled":false,"draft":""},{"id":1,"title":"B","provider":"Codex","settled":false,"draft":""}],"selected":1}}"#,
    ];
    for bytes in cases {
        fs::write(&path, bytes).unwrap();
        assert!(Store::open(&path).is_err(), "accepted {bytes}");
        assert_eq!(fs::read(&path).unwrap(), bytes.as_bytes());
    }
}

#[test]
fn save_failure_keeps_visible_state_unchanged() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.json");
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.path(), path.with_extension("sqlite3"));
    let before = Store::open(&path).unwrap().saved_snapshot().unwrap();
    let lock = Connection::open(store.path()).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(
        store
            .create(Provider::Claude, &working_directory(), 1)
            .is_err()
    );
    assert!(store.workspace().sessions().is_empty());
    assert_eq!(store.workspace().selected(), None);
    assert_eq!(store.writes(), 0);
    assert_eq!(
        Store::open(&path).unwrap().saved_snapshot().unwrap(),
        before
    );

    lock.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        store
            .create(Provider::Claude, &working_directory(), 1)
            .unwrap()
            .value,
        1
    );
    assert_eq!(store.writes(), 1);
    assert_eq!(
        Store::open(&path).unwrap().workspace().sessions()[0].id(),
        1
    );

    let saved = Store::open(&path).unwrap().saved_snapshot().unwrap();
    let before = store.workspace().clone();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(store.set_draft(1, "unwritten".into()).is_err());
    assert_eq!(store.workspace().sessions()[0].draft(), "");
    assert!(store.rename(1, "Unwritten").is_err());
    assert!(store.record_message(1, "Unwritten", 2).is_err());
    assert_eq!(
        store.workspace().sessions()[0].title(),
        "New Claude session"
    );
    assert_eq!(Store::open(&path).unwrap().saved_snapshot().unwrap(), saved);
    assert!(
        store
            .create(Provider::Codex, &working_directory(), 3)
            .is_err()
    );
    assert_eq!(store.workspace(), &before);
    assert_eq!(store.writes(), 1);
    assert_eq!(Store::open(&path).unwrap().saved_snapshot().unwrap(), saved);
    lock.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        store
            .create(Provider::Codex, &working_directory(), 3)
            .unwrap()
            .value,
        2
    );
    assert_eq!(store.writes(), 2);
    assert_eq!(store.workspace().sessions().len(), 2);
    assert_eq!(Store::open(&path).unwrap().workspace(), store.workspace());
}

#[test]
fn preferences_survive_restart_and_default_for_older_files() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.json");
    fs::write(
        &path,
        r#"{"version":1,"next_id":1,"workspace":{"sessions":[],"selected":null}}"#,
    )
    .unwrap();
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.workspace().preferences(), &Preferences::default());

    let appearance = Appearance {
        theme: ThemeMode::Light,
        density: Density::Compact,
        ui_font: Some("Noto Sans".into()),
        mono_font: Some("JetBrains Mono".into()),
        text_size: Some(15),
    };
    store.set_appearance(appearance.clone()).unwrap();
    drop(store);
    assert_eq!(
        Store::open(&path)
            .unwrap()
            .workspace()
            .preferences()
            .appearance(),
        &appearance
    );
}

#[test]
fn appearance_saved_before_mono_font_and_text_size_loads_with_their_defaults() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.json");
    fs::write(
        &path,
        r#"{"version":1,"next_id":1,"workspace":{"sessions":[],"selected":null,"preferences":{"theme":"Light","density":"Compact","ui_font":"Noto Sans"}}}"#,
    )
    .unwrap();
    assert_eq!(
        Store::open(&path)
            .unwrap()
            .workspace()
            .preferences()
            .appearance(),
        &Appearance {
            theme: ThemeMode::Light,
            density: Density::Compact,
            ui_font: Some("Noto Sans".into()),
            mono_font: None,
            text_size: None,
        }
    );
}

#[test]
fn provider_thread_survives_restart_and_is_absent_in_older_files() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.json");
    let mut store = Store::open(&path).unwrap();
    let id = store
        .create(Provider::Codex, &working_directory(), 1)
        .unwrap()
        .value;
    assert_eq!(store.workspace().sessions()[0].provider_thread(), None);
    let saved = Store::open(&path).unwrap().saved_snapshot().unwrap();
    assert!(
        saved["workspace"]["sessions"][0]
            .get("provider_thread")
            .is_none()
    );

    store.set_provider_thread(id, "thread-123".into()).unwrap();
    drop(store);
    let restored = Store::open(&path).unwrap();
    assert_eq!(
        restored.workspace().sessions()[0].provider_thread(),
        Some("thread-123")
    );
}

#[test]
fn approval_mode_defaults_to_provider_settings_and_survives_restart() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.json");
    // A session saved before approval modes existed keeps the provider's own settings.
    fs::write(
        &path,
        r#"{"version":1,"next_id":2,"workspace":{"sessions":[{"id":1,"title":"Old","provider":"Codex","settled":false,"draft":""}],"selected":1}}"#,
    )
    .unwrap();
    let mut store = Store::open(&path).unwrap();
    let session = &store.workspace().sessions()[0];
    assert_eq!(session.approval_mode(), ApprovalMode::Provider);
    assert!(!session.approval_overridden());

    let id = store
        .create(Provider::Claude, &working_directory(), 1)
        .unwrap()
        .value;
    assert_eq!(
        store.workspace().sessions()[1].approval_mode(),
        ApprovalMode::Provider
    );
    let saved = Store::open(&path).unwrap().saved_snapshot().unwrap();
    assert!(
        saved["workspace"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|session| session.get("approval_mode").is_none()
                && session.get("approval_overridden").is_none())
    );

    store.set_approval_mode(1, ApprovalMode::ReadOnly).unwrap();
    store.set_approval_overridden(1).unwrap();
    store
        .set_approval_mode(id, ApprovalMode::FullAccess)
        .unwrap();
    drop(store);
    let restored = Store::open(&path).unwrap();
    let sessions = restored.workspace().sessions();
    assert_eq!(sessions[0].approval_mode(), ApprovalMode::ReadOnly);
    assert!(sessions[0].approval_overridden());
    assert_eq!(sessions[1].approval_mode(), ApprovalMode::FullAccess);
    // Saved in the same write as the mode, before any turn could send the override.
    assert!(sessions[1].approval_overridden());

    // Returning to the default keeps the record that an override was sent.
    let mut store = restored;
    store.set_approval_mode(1, ApprovalMode::Provider).unwrap();
    let session = &Store::open(&path).unwrap().workspace().sessions()[0].clone();
    assert_eq!(session.approval_mode(), ApprovalMode::Provider);
    assert!(session.approval_overridden());
    assert!(store.set_approval_mode(99, ApprovalMode::Auto).is_err());
}

fn working_directory() -> WorkingDirectory {
    WorkingDirectory::new(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap()
}

#[test]
fn creation_atomically_records_context_and_restores_it_without_filesystem_lookup() {
    use keystone_core::{ContextOrigin, SessionContext};
    let dir = tempdir().unwrap();
    let project = dir.path().join("project 🥔");
    fs::create_dir(&project).unwrap();
    let directory = WorkingDirectory::new(&project).unwrap();
    assert_eq!(directory.path(), project.canonicalize().unwrap());
    let path = dir.path().join("state.json");
    let mut store = Store::open(&path).unwrap();
    let created = store.create(Provider::Codex, &directory, 42).unwrap();
    assert_eq!(created.value, 1);
    assert_eq!(store.writes(), 1);
    assert_eq!(store.workspace().selected(), Some(1));
    assert_eq!(store.workspace().sessions().len(), 1);
    assert_eq!(
        store.workspace().sessions()[0].context(),
        SessionContext::Recorded {
            directory: &directory,
            origin: ContextOrigin::Created,
        }
    );
    let saved = Store::open(&path).unwrap().saved_snapshot().unwrap();
    assert_eq!(saved["version"], 1);
    assert_eq!(saved["next_id"], 2);
    assert_eq!(saved["workspace"]["selected"], 1);
    assert_eq!(
        saved["workspace"]["sessions"][0]["execution_context"],
        serde_json::json!({
            "version": 1, "directory": directory.as_str(), "origin": "created"
        })
    );
    fs::remove_dir(&project).unwrap();
    assert!(WorkingDirectory::new(&project).is_err());
    let reopened = Store::open(&path).unwrap();
    assert_eq!(reopened.workspace(), store.workspace());
}

#[test]
fn working_directory_rejects_files_missing_paths_and_lossy_utf8() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("file");
    fs::write(&file, "").unwrap();
    assert!(WorkingDirectory::new(&file).is_err());
    assert!(WorkingDirectory::new(&dir.path().join("missing")).is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        // macOS file systems refuse a name that is not UTF-8, so no such folder exists there.
        #[cfg(not(target_os = "macos"))]
        {
            use std::os::unix::ffi::OsStringExt;
            let bad = dir.path().join(std::ffi::OsString::from_vec(vec![0xff]));
            fs::create_dir(&bad).unwrap();
            assert!(WorkingDirectory::new(&bad).is_err());
        }
        let alias = dir.path().join("alias");
        symlink(dir.path(), &alias).unwrap();
        assert_eq!(
            WorkingDirectory::new(&alias).unwrap(),
            WorkingDirectory::new(dir.path()).unwrap()
        );
    }
}

#[test]
fn missing_and_unsupported_contexts_survive_unrelated_saves_and_refuse_choice() {
    use keystone_core::SessionContext;
    use serde_json::{Value, json};
    let dir = tempdir().unwrap();
    let directory = WorkingDirectory::new(dir.path()).unwrap();
    let supported = json!({"version":1,"directory":directory.as_str(),"origin":"created"});
    let cases = [
        Value::Null,
        json!({"version":2,"directory":"/future","origin":"created","new":{"keep":true}}),
        json!({"version":1,"directory":"/future","origin":"inherited"}),
        json!({"version":1,"directory":"/future","origin":"chosen","policy":{"keep":1}}),
        json!({"version":1,"directory":"relative","origin":"created"}),
        json!({"version":1,"directory":17,"origin":"created"}),
        json!({"directory":"/future","origin":"created"}),
        json!(["future",{"nested":true}]),
        json!("opaque"),
        json!(false),
        json!({"version":1.5,"directory":"/future","origin":"created"}),
    ];
    for (index, raw) in cases.into_iter().enumerate() {
        let path = dir.path().join(format!("state-{index}.json"));
        let mut snapshot = json!({
            "version":1,"next_id":4,"future_root":{"keep":1},
            "workspace":{
                "selected":1,"future_workspace":[1,2],
                "preferences":{"future_preference":{"keep":2},"project_layouts":{"/future":{"version":99}}},
                "sessions":[
                    {"id":1,"title":"Legacy","provider":"Claude","settled":false,"draft":"keep","future_session":{"keep":3}},
                    {"id":2,"title":"Unknown","provider":"Codex","settled":false,"draft":"keep","execution_context":raw},
                    {"id":3,"title":"Recognized former unknown","provider":"Codex","settled":false,"draft":"keep","execution_context":supported}
                ]
            }
        });
        fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
        let legacy = fs::read(&path).unwrap();
        let mut store = Store::open(&path).unwrap();
        assert_eq!(
            store.workspace().sessions()[0].context(),
            SessionContext::LegacyMissing
        );
        assert_eq!(
            store.workspace().sessions()[1].context(),
            SessionContext::Unsupported
        );
        assert!(matches!(
            store.workspace().sessions()[2].context(),
            SessionContext::Recorded { .. }
        ));
        let before = Store::open(&path).unwrap().saved_snapshot().unwrap();
        let workspace = store.workspace().clone();
        assert!(store.choose_legacy_directory(2, &directory).is_err());
        assert_eq!(store.workspace(), &workspace);
        assert_eq!(
            Store::open(&path).unwrap().saved_snapshot().unwrap(),
            before
        );
        store.set_draft(1, "edited".into()).unwrap();
        store
            .set_appearance(Appearance {
                theme: ThemeMode::Light,
                ..Appearance::default()
            })
            .unwrap();
        snapshot["workspace"]["sessions"][0]["draft"] = json!("edited");
        let saved = Store::open(&path).unwrap().saved_snapshot().unwrap();
        assert_eq!(saved["future_root"], snapshot["future_root"]);
        assert_eq!(
            saved["workspace"]["future_workspace"],
            snapshot["workspace"]["future_workspace"]
        );
        assert_eq!(
            saved["workspace"]["preferences"]["future_preference"],
            snapshot["workspace"]["preferences"]["future_preference"]
        );
        assert_eq!(
            saved["workspace"]["preferences"]["project_layouts"],
            snapshot["workspace"]["preferences"]["project_layouts"]
        );
        assert_eq!(
            saved["workspace"]["sessions"],
            snapshot["workspace"]["sessions"]
        );
        assert!(
            saved["workspace"]["sessions"][0]
                .get("execution_context")
                .is_none()
        );
        assert_eq!(Store::open(&path).unwrap().workspace(), store.workspace());
        assert_eq!(fs::read(&path).unwrap(), legacy);
    }
}

#[test]
fn legacy_choice_changes_only_context_and_refuses_replacement() {
    use keystone_core::{ContextOrigin, SessionContext};
    use serde_json::{Value, json};
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.json");
    let directory = WorkingDirectory::new(dir.path()).unwrap();
    fs::write(
        &path,
        serde_json::to_vec(&json!({
            "version":1,"next_id":2,"workspace":{"selected":null,"sessions":[{
                "id":1,"title":"Legacy","provider":"Claude","settled":true,"draft":"keep",
                "provider_thread":"thread-1","created_at":5,"updated_at":9,"viewed_at":7,
                "last_turn":"Finished","approval_mode":"Ask","approval_overridden":true,
                "future_session":{"keep":true}
            }],"preferences":{"future_preference":true}}
        }))
        .unwrap(),
    )
    .unwrap();
    let legacy = fs::read(&path).unwrap();
    let mut store = Store::open(&path).unwrap();
    let mut expected: Value = serde_json::to_value(store.workspace()).unwrap();
    store.choose_legacy_directory(1, &directory).unwrap();
    expected["sessions"][0]["execution_context"] = json!({
        "version":1,"directory":directory.as_str(),"origin":"chosen"
    });
    assert_eq!(serde_json::to_value(store.workspace()).unwrap(), expected);
    assert_eq!(
        store.workspace().sessions()[0].context(),
        SessionContext::Recorded {
            directory: &directory,
            origin: ContextOrigin::Chosen
        }
    );
    let before = Store::open(&path).unwrap().saved_snapshot().unwrap();
    assert!(store.choose_legacy_directory(99, &directory).is_err());
    store.choose_legacy_directory(1, &directory).unwrap();
    assert_eq!(store.writes(), 1);
    assert_eq!(
        Store::open(&path).unwrap().saved_snapshot().unwrap(),
        before
    );
    assert!(
        store
            .choose_legacy_directory(1, &working_directory())
            .is_err()
    );
    assert_eq!(serde_json::to_value(store.workspace()).unwrap(), expected);
    assert_eq!(
        Store::open(&path).unwrap().saved_snapshot().unwrap(),
        before
    );
    store.create(Provider::Codex, &directory, 10).unwrap();
    let before = Store::open(&path).unwrap().saved_snapshot().unwrap();
    let expected = store.workspace().clone();
    assert!(store.choose_legacy_directory(2, &directory).is_err());
    assert_eq!(store.workspace(), &expected);
    assert_eq!(
        Store::open(&path).unwrap().saved_snapshot().unwrap(),
        before
    );
    assert_eq!(Store::open(&path).unwrap().workspace(), store.workspace());
    assert_eq!(fs::read(&path).unwrap(), legacy);
}

#[test]
fn failed_legacy_choice_before_rename_keeps_context_and_retries() {
    use keystone_core::SessionContext;
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.json");
    fs::write(&path, r#"{"version":1,"next_id":2,"workspace":{"selected":1,"sessions":[{"id":1,"title":"Legacy","provider":"Codex","settled":false,"draft":"keep"}]}}"#).unwrap();
    let legacy = fs::read(&path).unwrap();
    let mut store = Store::open(&path).unwrap();
    let before = store.workspace().clone();
    let saved = Store::open(&path).unwrap().saved_snapshot().unwrap();
    let lock = Connection::open(store.path()).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(
        store
            .choose_legacy_directory(1, &working_directory())
            .is_err()
    );
    assert_eq!(store.workspace(), &before);
    assert_eq!(
        store.workspace().sessions()[0].context(),
        SessionContext::LegacyMissing
    );
    assert_eq!(store.writes(), 0);
    assert_eq!(Store::open(&path).unwrap().workspace(), &before);
    assert_eq!(Store::open(&path).unwrap().saved_snapshot().unwrap(), saved);
    assert_eq!(fs::read(&path).unwrap(), legacy);
    lock.execute_batch("ROLLBACK").unwrap();
    store
        .choose_legacy_directory(1, &working_directory())
        .unwrap();
    assert_eq!(store.writes(), 1);
    assert_eq!(Store::open(&path).unwrap().workspace(), store.workspace());
    let saved = Store::open(&path).unwrap().saved_snapshot().unwrap();
    store
        .choose_legacy_directory(1, &working_directory())
        .unwrap();
    assert_eq!(store.writes(), 1);
    assert_eq!(Store::open(&path).unwrap().saved_snapshot().unwrap(), saved);
    assert_eq!(fs::read(&path).unwrap(), legacy);
}
