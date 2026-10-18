use keystone_core::{Appearance, ApprovalMode, Density, Preferences, Provider, Store, ThemeMode};
use std::fs;
use tempfile::tempdir;

#[test]
fn sessions_and_drafts_survive_restart() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.json");
    let mut store = Store::open(&path).unwrap();
    assert!(store.workspace().sessions().is_empty());
    assert_eq!(store.workspace().selected(), None);

    let claude = store.create(Provider::Claude, 1).unwrap().value;
    let codex = store.create(Provider::Codex, 1).unwrap().value;
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
    assert_eq!(restored.create(Provider::Claude, 1).unwrap().value, 3);
}

#[test]
fn invalid_commands_leave_memory_and_file_unchanged() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.json");
    let mut store = Store::open(&path).unwrap();
    store.create(Provider::Claude, 1).unwrap();
    let before = fs::read(&path).unwrap();

    assert!(store.select(99).is_err());
    assert!(store.set_settled(99, true).is_err());
    assert!(store.set_draft(99, "lost".into()).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
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
    let path = dir.path().join("missing").join("state.json");
    let mut store = Store::open(&path).unwrap();
    assert!(store.create(Provider::Claude, 1).is_err());
    assert!(store.workspace().sessions().is_empty());
    assert_eq!(store.workspace().selected(), None);

    fs::create_dir(path.parent().unwrap()).unwrap();
    assert_eq!(store.create(Provider::Claude, 1).unwrap().value, 1);
    assert_eq!(
        Store::open(&path).unwrap().workspace().sessions()[0].id(),
        1
    );

    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(store.set_draft(1, "unwritten".into()).is_err());
    assert_eq!(store.workspace().sessions()[0].draft(), "");
    assert!(store.rename(1, "Unwritten").is_err());
    assert!(store.record_message(1, "Unwritten", 2).is_err());
    assert_eq!(
        store.workspace().sessions()[0].title(),
        "New Claude session"
    );
    assert!(path.is_dir());
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
fn provider_thread_survives_restart_and_is_absent_in_older_files() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.json");
    let mut store = Store::open(&path).unwrap();
    let id = store.create(Provider::Codex, 1).unwrap().value;
    assert_eq!(store.workspace().sessions()[0].provider_thread(), None);
    assert!(
        !fs::read_to_string(&path)
            .unwrap()
            .contains("provider_thread")
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

    let id = store.create(Provider::Claude, 1).unwrap().value;
    assert_eq!(
        store.workspace().sessions()[1].approval_mode(),
        ApprovalMode::Provider
    );
    let saved = fs::read_to_string(&path).unwrap();
    assert!(!saved.contains("approval_mode") && !saved.contains("approval_overridden"));

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
