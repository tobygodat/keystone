use super::*;
use std::path::Path;

fn working_directory() -> WorkingDirectory {
    WorkingDirectory::new(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn fields(title: &str, body: &str, shortcut: &str, project: Option<&ProjectId>) -> PromptFields {
    PromptFields {
        title: title.into(),
        body: body.into(),
        shortcut: shortcut.into(),
        project: project.cloned(),
    }
}

fn events(store: &Store) -> Vec<(String, String)> {
    store
        .database()
        .events_after(0, 1000)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind.starts_with("prompt."))
        .map(|event| (event.kind, event.entity_id))
        .collect()
}

fn titles(store: &Store) -> Vec<String> {
    store
        .workspace()
        .prompts()
        .iter()
        .map(|p| p.title().to_owned())
        .collect()
}

#[test]
fn prompts_are_added_edited_reordered_counted_and_removed_through_the_event_log() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    let project = ProjectId::new(directory.path()).unwrap();
    let mut store = Store::open(&path).unwrap();
    let session = store
        .create(Provider::Claude, &working_directory(), 1)
        .unwrap()
        .value;
    let review = store
        .add_prompt(fields(
            "Review",
            "Review this diff.\nBe terse.",
            "/rev",
            None,
        ))
        .unwrap()
        .value;
    let plan = store
        .add_prompt(fields("Plan", "Plan first.", "", Some(&project)))
        .unwrap()
        .value;
    assert!(review > session && plan > review);
    let next = store
        .create(Provider::Codex, &working_directory(), 2)
        .unwrap()
        .value;
    assert!(next > plan, "sessions and prompts never share an ID");

    store
        .edit_prompt(
            review,
            fields("Review closely", "Review this diff.", "review", None),
        )
        .unwrap();
    store.move_prompt(plan, 0).unwrap();
    store.count_prompt_use(review).unwrap();
    store.count_prompt_use(review).unwrap();

    let reopened = Store::open(&path).unwrap();
    assert_eq!(titles(&reopened), ["Plan", "Review closely"]);
    let saved = reopened.workspace().prompt(review).unwrap();
    assert_eq!(saved.body(), "Review this diff.");
    assert_eq!(saved.shortcut(), Some("review"));
    assert_eq!(saved.project(), None);
    assert_eq!(saved.uses(), 2);
    assert_eq!(
        reopened.workspace().prompt(plan).unwrap().project(),
        Some(project)
    );

    store.remove_prompt(plan).unwrap();
    assert!(store.remove_prompt(plan).is_err());
    assert_eq!(titles(&Store::open(&path).unwrap()), ["Review closely"]);
    let events = events(&store);
    assert_eq!(
        events.first(),
        Some(&("prompt.upserted".into(), review.to_string()))
    );
    // Removing the first prompt also moves the one after it up.
    assert_eq!(
        events[events.len() - 2..],
        [
            ("prompt.removed".into(), plan.to_string()),
            ("prompt.upserted".into(), review.to_string())
        ]
    );
    let saved = store.saved_snapshot().unwrap();
    assert!(
        saved["workspace"]["prompts"].is_array(),
        "the projection loads into the workspace"
    );
}

#[test]
fn a_prompt_needs_a_title_and_text_and_a_one_word_shortcut_no_overlapping_prompt_uses() {
    let directory = tempfile::tempdir().unwrap();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let (a, b) = (
        ProjectId::new(a.path()).unwrap(),
        ProjectId::new(b.path()).unwrap(),
    );
    let mut store = Store::open(directory.path().join("state.json")).unwrap();
    let refused =
        |store: &mut Store, f: PromptFields| store.add_prompt(f).err().unwrap().to_string();

    assert!(refused(&mut store, fields(" \n", "x", "", None)).contains("title"));
    assert!(refused(&mut store, fields("T", " \n ", "", None)).contains("text"));
    assert!(refused(&mut store, fields("T", "x", "/two words", None)).contains("one word"));
    assert!(refused(&mut store, fields("T", "x", "/über", None)).contains("one word"));

    let first = store
        .add_prompt(fields("  Multi\n line  ", "body", " /fix ", Some(&a)))
        .unwrap()
        .value;
    let saved = store.workspace().prompt(first).unwrap();
    assert_eq!(saved.title(), "Multi line");
    assert_eq!(saved.shortcut(), Some("fix"));

    // Another project's sessions never see `a`'s prompt, so the shortcut is free there.
    store
        .add_prompt(fields("Other", "body", "fix", Some(&b)))
        .unwrap();
    let message = refused(&mut store, fields("Global", "body", "/FIX", None));
    assert!(message.contains("/FIX already inserts"), "{message}");
    assert!(refused(&mut store, fields("Same", "body", "fix", Some(&a))).contains("already"));

    // Editing a prompt keeps its own shortcut.
    store
        .edit_prompt(first, fields("Renamed", "body", "fix", Some(&a)))
        .unwrap();
    assert!(store.edit_prompt(999, fields("T", "x", "", None)).is_err());
}

#[test]
fn a_global_prompt_shows_in_every_project_and_a_project_prompt_only_in_its_own() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("project");
    std::fs::create_dir_all(project.join("nested")).unwrap();
    let id = ProjectId::new(&project).unwrap();
    let nested = ProjectId::new(&project.join("nested")).unwrap();
    let parent = ProjectId::new(directory.path()).unwrap();
    let mut store = Store::open(directory.path().join("state.json")).unwrap();
    store.add_prompt(fields("G", "g", "", None)).unwrap();
    store.add_prompt(fields("P", "p", "", Some(&id))).unwrap();
    let shown = |project: Option<&ProjectId>| -> Vec<&str> {
        store
            .workspace()
            .prompts()
            .iter()
            .filter(|p| p.shown_in(project))
            .map(|p| p.title())
            .collect()
    };
    assert_eq!(shown(Some(&id)), ["G", "P"]);
    assert_eq!(shown(Some(&nested)), ["G"]);
    assert_eq!(shown(Some(&parent)), ["G"]);
    assert_eq!(shown(None), ["G"]);
}

/// A database from before projects (version 2) and one a projects build left at
/// version 3, with its `projects` rows, both gain an empty prompt list and keep
/// everything else.
#[test]
fn schema_2_and_projects_schema_3_databases_gain_an_empty_prompt_list() {
    for (version, removed) in [
        (
            2,
            "DROP TABLE notes; DROP TABLE deliveries; DROP TABLE prompts; DROP TABLE projects;",
        ),
        (
            3,
            "DROP TABLE notes; DROP TABLE deliveries; DROP TABLE prompts;",
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        drop_store(&path);
        let database = path.with_extension("sqlite3");
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch(&format!("{removed} PRAGMA user_version={version};"))
            .unwrap();
        if version == 3 {
            connection
                .execute(
                    r#"INSERT INTO projects VALUES ('/kept', 0, '{"path":"/kept"}')"#,
                    [],
                )
                .unwrap();
        }
        drop(connection);
        let mut store = Store::open(&path).unwrap();
        assert!(store.workspace().prompts().is_empty());
        store.add_prompt(fields("T", "x", "", None)).unwrap();
        assert_eq!(titles(&Store::open(&path).unwrap()), ["T"]);
        let connection = rusqlite::Connection::open(&database).unwrap();
        let schema: u32 = connection
            .query_one("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(schema, crate::database::SCHEMA_VERSION);
        let projects: i64 = connection
            .query_one("SELECT count(*) FROM projects", [], |row| row.get(0))
            .unwrap();
        assert_eq!(projects, i64::from(version == 3), "version {version}");
    }
}

fn drop_store(path: &Path) {
    drop(Store::open(path).unwrap());
}

#[test]
fn a_saved_prompt_list_with_a_duplicate_or_session_id_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    let mut store = Store::open(&path).unwrap();
    store
        .create(Provider::Claude, &working_directory(), 1)
        .unwrap();
    let prompt = store.add_prompt(fields("T", "x", "", None)).unwrap().value;
    drop(store);
    let database = path.with_extension("sqlite3");
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute(
            "UPDATE prompts SET payload=json_set(payload, '$.id', 1)",
            [],
        )
        .unwrap();
    assert!(
        Store::open(&path)
            .err()
            .unwrap()
            .to_string()
            .contains("prompt ID")
    );
    connection
        .execute(
            "UPDATE prompts SET payload=json_set(payload, '$.id', ?1)",
            [prompt as i64],
        )
        .unwrap();
    Store::open(&path).unwrap();

    // Two rows holding the same prompt ID.
    connection
        .execute(
            "INSERT INTO prompts SELECT 'copy', position + 1, payload FROM prompts",
            [],
        )
        .unwrap();
    assert!(
        Store::open(&path)
            .err()
            .unwrap()
            .to_string()
            .contains("duplicate prompt ID")
    );
}
