use keystone_core::{Provider, Store, WorkingDirectory};

#[test]
fn a_stale_writer_never_overwrites_another_stores_session() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    let directory = WorkingDirectory::new(dir.path()).unwrap();
    let mut first = Store::open(&path).unwrap();
    let mut stale = Store::open(&path).unwrap();
    first.create(Provider::Claude, &directory, 1).unwrap();
    stale.create(Provider::Codex, &directory, 2).unwrap();
    stale.flush().unwrap();
    assert!(matches!(
        stale.save_error(),
        Some(keystone_core::SaveError::Stale(_))
    ));
    assert_eq!(stale.workspace().sessions().len(), 1);
    let restored = Store::open(&path).unwrap();
    assert_eq!(restored.workspace().sessions().len(), 1);
    assert_eq!(
        restored.workspace().sessions()[0].provider(),
        Provider::Claude
    );
}

#[test]
fn saving_an_imported_workspace_never_mutates_the_legacy_backup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    let original = r#"{"version":1,"next_id":2,"workspace":{"selected":1,"sessions":[{"id":1,"title":"Legacy","provider":"Codex","settled":false,"draft":"backup"}]}}"#;
    std::fs::write(&path, original).unwrap();
    let mut store = Store::open(&path).unwrap();
    store.set_draft(1, "new SQLite draft".into()).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    assert_eq!(
        Store::open(&path).unwrap().workspace().sessions()[0].draft(),
        "new SQLite draft"
    );
}
