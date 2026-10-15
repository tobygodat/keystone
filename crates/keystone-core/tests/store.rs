use keystone_core::{Provider, Store};
use std::fs;
use tempfile::tempdir;

#[test]
fn sessions_and_drafts_survive_restart() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.json");
    let mut store = Store::open(&path).unwrap();
    assert!(store.workspace().sessions().is_empty());
    assert_eq!(store.workspace().selected(), None);

    let claude = store.create(Provider::Claude).unwrap();
    let codex = store.create(Provider::Codex).unwrap();
    assert_eq!((claude, codex), (1, 2));
    store.select(claude).unwrap();
    store.set_settled(codex, true).unwrap();
    store.set_draft(claude, "a saved thought".into()).unwrap();
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
    assert_eq!(sessions[0].draft(), "a saved thought");
    assert_eq!(sessions[1].title(), "New Codex session");
    assert_eq!(sessions[1].provider(), Provider::Codex);
    assert!(sessions[1].settled());
    assert_eq!(sessions[1].draft(), "");

    restored.set_settled(codex, false).unwrap();
    assert!(!Store::open(&path).unwrap().workspace().sessions()[1].settled());
    assert_eq!(restored.create(Provider::Claude).unwrap(), 3);
}

#[test]
fn invalid_commands_leave_memory_and_file_unchanged() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("state.json");
    let mut store = Store::open(&path).unwrap();
    store.create(Provider::Claude).unwrap();
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
    assert!(store.create(Provider::Claude).is_err());
    assert!(store.workspace().sessions().is_empty());
    assert_eq!(store.workspace().selected(), None);

    fs::create_dir(path.parent().unwrap()).unwrap();
    assert_eq!(store.create(Provider::Claude).unwrap(), 1);
    assert_eq!(
        Store::open(&path).unwrap().workspace().sessions()[0].id(),
        1
    );

    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(store.set_draft(1, "unwritten".into()).is_err());
    assert_eq!(store.workspace().sessions()[0].draft(), "");
    assert!(path.is_dir());
}
