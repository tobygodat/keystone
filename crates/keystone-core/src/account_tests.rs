use super::*;
use std::path::Path;

fn working_directory() -> WorkingDirectory {
    WorkingDirectory::new(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn kinds(store: &Store, after: u64) -> Vec<(String, String)> {
    store
        .database()
        .events_after(after, 1000)
        .unwrap()
        .into_iter()
        .map(|event| (event.kind, event.entity_id))
        .collect()
}

fn last_sequence(store: &Store) -> u64 {
    store
        .database()
        .events_after(0, 1000)
        .unwrap()
        .last()
        .map_or(0, |event| event.sequence)
}

#[test]
fn old_workspaces_load_every_session_on_the_default_account_and_keep_it_fixed() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    std::fs::write(&path, r#"{"version":1,"next_id":3,"workspace":{"selected":1,"sessions":[{"id":1,"title":"Old","provider":"Claude","settled":false,"draft":"","provider_thread":"thread-1"},{"id":2,"title":"New","provider":"Codex","settled":false,"draft":"","model_locked":false}]}}"#).unwrap();
    let mut store = Store::open(&path).unwrap();
    let sessions = store.workspace().sessions();
    assert!(sessions.iter().all(|session| session.account().is_none()));
    assert!(store.workspace().accounts().is_empty());
    let home = directory.path().join("claude-work");
    let work = store
        .add_account(Provider::Claude, "Work", home, &[])
        .unwrap()
        .value;
    let started = store.set_account(1, Some(work.clone())).unwrap_err();
    assert!(started.to_string().contains("fixed once"), "{started}");
    assert!(store.workspace().sessions()[0].account().is_none());
    let reopened = Store::open(&path).unwrap();
    assert!(reopened.workspace().sessions()[0].account().is_none());
    assert_eq!(reopened.workspace().accounts().len(), 1);
}

#[test]
fn accounts_persist_in_their_own_projection_with_events() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    let mut store = Store::open(&path).unwrap();
    let before = last_sequence(&store);
    let home = directory.path().join("max-2");
    let id = store
        .add_account(Provider::Claude, "  Max   two ", home.clone(), &[])
        .unwrap()
        .value;
    assert_eq!(id.as_str(), "claude-max-two");
    assert_eq!(
        kinds(&store, before),
        vec![("account.upserted".into(), "claude-max-two".into())]
    );
    let workspace_payload: String = rusqlite::Connection::open(store.path())
        .unwrap()
        .query_one("SELECT payload FROM workspace", [], |row| row.get(0))
        .unwrap();
    assert!(
        !workspace_payload.contains("accounts"),
        "{workspace_payload}"
    );

    let renamed = last_sequence(&store);
    store.rename_account(&id, "Personal Max").unwrap();
    store.rename_account(&id, "Personal Max").unwrap();
    assert_eq!(
        kinds(&store, renamed),
        vec![("account.upserted".into(), "claude-max-two".into())]
    );
    let reopened = Store::open(&path).unwrap();
    let account = &reopened.workspace().accounts()[0];
    assert_eq!(
        (
            account.id(),
            account.provider(),
            account.label(),
            account.home()
        ),
        (&id, Provider::Claude, "Personal Max", home.as_path())
    );

    let removed = last_sequence(&store);
    store.remove_account(&id).unwrap();
    assert_eq!(
        kinds(&store, removed),
        vec![("account.removed".into(), "claude-max-two".into())]
    );
    assert!(
        Store::open(&path)
            .unwrap()
            .workspace()
            .accounts()
            .is_empty()
    );
}

#[test]
fn account_homes_never_overlap_and_labels_are_unique_per_provider() {
    let mut store = Store::in_memory().unwrap();
    let reserved = [
        PathBuf::from("/home/u/.claude"),
        PathBuf::from("/home/u/.codex"),
    ];
    store
        .add_account(
            Provider::Claude,
            "Work",
            "/home/u/.claude-work".into(),
            &reserved,
        )
        .unwrap();
    for (provider, label, home) in [
        (Provider::Claude, "Again", "/home/u/.claude-work"),
        (Provider::Codex, "Inside", "/home/u/.claude-work/codex"),
        (Provider::Claude, "Parent", "/home/u"),
        (Provider::Claude, "Default", "/home/u/.claude"),
        (Provider::Codex, "Nested default", "/home/u/.codex/other"),
        (Provider::Claude, "work", "/home/u/elsewhere"),
        (Provider::Claude, "  ", "/home/u/blank"),
        (Provider::Claude, "Relative", "relative"),
    ] {
        assert!(
            store
                .add_account(provider, label, home.into(), &reserved)
                .is_err(),
            "{label} at {home} must be refused"
        );
    }
    store
        .add_account(
            Provider::Codex,
            "Work",
            "/home/u/.codex-work".into(),
            &reserved,
        )
        .unwrap();
    assert_eq!(store.workspace().accounts().len(), 2);
}

#[test]
fn a_session_account_is_chosen_before_the_first_send_and_fixed_after() {
    let mut store = Store::in_memory().unwrap();
    let claude = store
        .add_account(
            Provider::Claude,
            "Work",
            "/accounts/claude-work".into(),
            &[],
        )
        .unwrap()
        .value;
    let codex = store
        .add_account(Provider::Codex, "Work", "/accounts/codex-work".into(), &[])
        .unwrap()
        .value;
    let id = store
        .create(Provider::Claude, &working_directory(), 1)
        .unwrap()
        .value;
    assert!(store.set_account(id, Some(codex.clone())).is_err());
    assert!(
        store
            .set_account(id, Some(AccountId::new("missing")))
            .is_err()
    );
    store.set_account(id, Some(claude.clone())).unwrap();
    assert_eq!(store.workspace().sessions()[0].account(), Some(&claude));
    store
        .set_account(id, Some(AccountId::new(AccountId::DEFAULT)))
        .unwrap();
    assert_eq!(store.workspace().sessions()[0].account(), None);
    store.set_account(id, Some(claude.clone())).unwrap();
    store.set_provider(id, Provider::Claude).unwrap();
    assert_eq!(store.workspace().sessions()[0].account(), Some(&claude));
    store.set_provider(id, Provider::Codex).unwrap();
    assert_eq!(store.workspace().sessions()[0].account(), None);
    store.set_account(id, Some(codex.clone())).unwrap();

    let refused = store.remove_account(&codex).unwrap_err();
    assert!(refused.to_string().contains("1 session uses"), "{refused}");
    store.lock_model(id).unwrap();
    assert!(store.set_account(id, None).is_err());
    assert!(store.set_provider(id, Provider::Claude).is_err());
    assert_eq!(store.workspace().sessions()[0].account(), Some(&codex));
    assert!(store.remove_account(&codex).is_err());
    store.remove_account(&claude).unwrap();
}

#[test]
fn a_schema_1_database_migrates_once_and_keeps_its_workspace() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    {
        let mut store = Store::open(&path).unwrap();
        store
            .create(Provider::Codex, &working_directory(), 7)
            .unwrap();
        store.set_draft(1, "kept".into()).unwrap();
    }
    let database = path.with_extension("sqlite3");
    {
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch("DROP TABLE accounts; DROP TABLE projects; PRAGMA user_version=1;")
            .unwrap();
    }
    assert!(Database::open_read_only(&path).is_err());
    let events_before = Database::open(&path)
        .unwrap()
        .events_after(0, 1000)
        .unwrap()
        .len();
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.workspace().sessions()[0].draft(), "kept");
    assert_eq!(store.workspace().sessions()[0].account(), None);
    let version: u32 = rusqlite::Connection::open(&database)
        .unwrap()
        .query_one("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, crate::database::SCHEMA_VERSION);
    assert_eq!(
        store.database().events_after(0, 1000).unwrap().len(),
        events_before
    );
    store
        .add_account(
            Provider::Codex,
            "Second",
            directory.path().join("codex-2"),
            &[],
        )
        .unwrap();
    assert_eq!(Store::open(&path).unwrap().workspace().accounts().len(), 1);
    Database::open_read_only(&path).unwrap();
}

#[test]
fn a_newer_schema_is_still_refused() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    drop(Store::open(&path).unwrap());
    rusqlite::Connection::open(path.with_extension("sqlite3"))
        .unwrap()
        .execute_batch("PRAGMA user_version=4;")
        .unwrap();
    let refused = Store::open(&path).err().unwrap();
    assert!(
        refused.to_string().contains("schema version 4"),
        "{refused}"
    );
}

#[test]
fn a_saved_account_list_with_a_reserved_or_duplicate_id_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    let mut store = Store::open(&path).unwrap();
    store
        .add_account(Provider::Claude, "Work", "/accounts/a".into(), &[])
        .unwrap();
    store
        .add_account(Provider::Claude, "Home", "/accounts/b".into(), &[])
        .unwrap();
    let connection = rusqlite::Connection::open(store.path()).unwrap();
    connection
        .execute(
            "UPDATE accounts SET payload=json_set(payload, '$.id', 'claude-work') WHERE id='claude-home'",
            [],
        )
        .unwrap();
    assert!(Store::open(&path).is_err());
    connection
        .execute(
            "UPDATE accounts SET payload=json_set(payload, '$.id', 'default') WHERE id='claude-home'",
            [],
        )
        .unwrap();
    assert!(Store::open(&path).is_err());
}

#[test]
fn an_account_cannot_be_named_default_or_take_the_default_rows_id() {
    let mut store = Store::in_memory().unwrap();
    for label in ["Default", "default", " DEFAULT "] {
        let refused = store
            .add_account(Provider::Claude, label, "/accounts/a".into(), &[])
            .unwrap_err()
            .to_string();
        assert!(refused.contains("Default"), "{refused}");
    }
    let id = store
        .add_account(Provider::Claude, "Default!", "/accounts/b".into(), &[])
        .unwrap()
        .value;
    assert_eq!(id.as_str(), "claude-default-2");
    assert_eq!(
        account::slug(Provider::Codex, "Default", &[]).as_str(),
        "codex-default-2"
    );
}

#[test]
fn another_account_returns_the_model_to_the_default_and_a_provider_move_saves_once() {
    let mut store = Store::in_memory().unwrap();
    let work = store
        .add_account(
            Provider::Claude,
            "Work",
            "/accounts/claude-work".into(),
            &[],
        )
        .unwrap()
        .value;
    let codex = store
        .add_account(Provider::Codex, "Work", "/accounts/codex-work".into(), &[])
        .unwrap()
        .value;
    let id = store
        .create(Provider::Claude, &working_directory(), 1)
        .unwrap()
        .value;
    store.set_model(id, Some("only-on-default".into())).unwrap();
    store.set_account(id, None).unwrap();
    assert_eq!(
        store.workspace().sessions()[0].model(),
        Some("only-on-default")
    );
    store.set_account(id, Some(work.clone())).unwrap();
    assert_eq!(store.workspace().sessions()[0].model(), None);

    store.set_model(id, Some("work-model".into())).unwrap();
    let before = last_sequence(&store);
    store
        .set_provider_account(id, Provider::Codex, Some(codex.clone()))
        .unwrap();
    let session = &store.workspace().sessions()[0];
    assert_eq!(
        (session.provider(), session.account(), session.model()),
        (Provider::Codex, Some(&codex), None)
    );
    assert_eq!(session.title, "New Codex session");
    assert_eq!(kinds(&store, before).len(), 1, "one save");
    assert!(
        store
            .set_provider_account(id, Provider::Claude, Some(codex))
            .is_err()
    );
    assert_eq!(store.workspace().sessions()[0].provider(), Provider::Codex);
}

#[test]
fn removing_or_adding_an_account_forgets_usage_saved_under_its_id() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    let mut store = Store::open(&path).unwrap();
    let id = store
        .add_account(Provider::Claude, "Work", "/accounts/a".into(), &[])
        .unwrap()
        .value;
    let mut usage = crate::AccountUsage::default();
    usage.record(
        vec![crate::LimitWindow {
            id: "five_hour".into(),
            label: "5-hour".into(),
            used_percent: 90.0,
            resets_at: None,
        }],
        1,
    );
    let key = format!("claude/{id}");
    store.database().record_usage(&key, &usage).unwrap();
    store
        .database()
        .record_usage("claude/default", &usage)
        .unwrap();
    store.remove_account(&id).unwrap();
    let keys = |store: &Store| -> Vec<String> {
        store
            .database()
            .usage()
            .unwrap()
            .into_iter()
            .map(|(key, _)| key)
            .collect()
    };
    assert_eq!(keys(&store), ["claude/default"]);
    // Saved under the ID by a build that did not clear it, then the ID is reused.
    store.database().record_usage(&key, &usage).unwrap();
    let again = store
        .add_account(Provider::Claude, "Work", "/accounts/b".into(), &[])
        .unwrap()
        .value;
    assert_eq!(again, id);
    assert_eq!(keys(&Store::open(&path).unwrap()), ["claude/default"]);
}
