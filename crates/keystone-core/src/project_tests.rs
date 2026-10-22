use super::*;
use crate::{Placement, Provider, WorkingDirectory};

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

fn listed(store: &Store) -> Vec<(ProjectId, Option<u64>)> {
    store
        .workspace()
        .projects()
        .into_iter()
        .map(|p| (p.id(), p.last_opened()))
        .collect()
}

fn folders(root: &Path) -> (ProjectId, ProjectId, ProjectId) {
    let make = |name: &str| {
        std::fs::create_dir(root.join(name)).unwrap();
        ProjectId::new(&root.join(name)).unwrap()
    };
    (make("a"), make("b"), make("c"))
}

#[test]
fn projects_persist_in_their_own_projection_most_recent_first() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    let (a, b, c) = folders(directory.path());
    let mut store = Store::open(&path).unwrap();
    let before = last_sequence(&store);
    let writes = store.writes();
    store
        .open_project(WindowId::FIRST, &b, Some(&a), None, 100)
        .unwrap();
    assert_eq!(store.writes(), writes + 1, "one write");
    assert_eq!(
        listed(&store),
        vec![(b.clone(), Some(100)), (a.clone(), Some(99))]
    );
    let events = kinds(&store, before);
    let id = |p: &ProjectId| p.path().to_string_lossy().into_owned();
    assert!(
        events.contains(&("project.upserted".into(), id(&a))),
        "{events:?}"
    );
    assert!(
        events.contains(&("project.upserted".into(), id(&b))),
        "{events:?}"
    );
    let workspace_payload: String = rusqlite::Connection::open(store.path())
        .unwrap()
        .query_one("SELECT payload FROM workspace", [], |row| row.get(0))
        .unwrap();
    assert!(
        !workspace_payload.contains("last_opened"),
        "{workspace_payload}"
    );

    store
        .open_project(WindowId::FIRST, &c, Some(&b), None, 200)
        .unwrap();
    let reopened = last_sequence(&store);
    store
        .open_project(WindowId::FIRST, &a, Some(&c), None, 300)
        .unwrap();
    assert_eq!(
        kinds(&store, reopened),
        vec![
            ("project.upserted".into(), id(&a)),
            ("workspace.upserted".into(), "workspace".into()),
        ],
        "returning to a listed project changes only its row"
    );
    let reopened = Store::open(&path).unwrap();
    assert_eq!(
        listed(&reopened),
        vec![
            (a.clone(), Some(300)),
            (c.clone(), Some(200)),
            (b, Some(100))
        ]
    );
    assert_eq!(
        reopened
            .workspace()
            .preferences()
            .window_project(WindowId::FIRST),
        Some(a)
    );
}

#[test]
fn forgetting_a_project_keeps_its_folder_sessions_and_layout() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    let (a, b, _) = folders(directory.path());
    std::fs::write(b.path().join("kept.txt"), "mine").unwrap();
    let mut store = Store::open(&path).unwrap();
    let in_b = WorkingDirectory::new(b.path()).unwrap();
    store.create(Provider::Codex, &in_b, 1).unwrap();
    store
        .set_layout_in(
            &b,
            WindowId::FIRST,
            Some(crate::Composition::default().into()),
        )
        .unwrap();
    store
        .open_project(WindowId::FIRST, &a, Some(&b), None, 10)
        .unwrap();
    let before = last_sequence(&store);
    store.forget_project(&b).unwrap();
    assert_eq!(
        kinds(&store, before)[0],
        (
            "project.removed".into(),
            b.path().to_string_lossy().into_owned()
        )
    );
    let err = store.forget_project(&b).unwrap_err();
    assert!(err.to_string().contains("not on the project list"), "{err}");

    let reopened = Store::open(&path).unwrap();
    assert_eq!(listed(&reopened), vec![(a.clone(), Some(10))]);
    assert_eq!(reopened.workspace().sessions().len(), 1);
    assert!(
        reopened
            .workspace()
            .preferences()
            .project_layout(&b)
            .is_some()
    );
    assert_eq!(
        std::fs::read_to_string(b.path().join("kept.txt")).unwrap(),
        "mine"
    );
    assert_eq!(
        reopened.workspace().removed_projects().collect::<Vec<_>>(),
        vec![b.clone()],
        "the removal is remembered, so its sessions' folder is not offered again"
    );

    let mut reopened = reopened;
    reopened
        .open_project(WindowId::FIRST, &b, Some(&a), None, 20)
        .unwrap();
    assert_eq!(listed(&reopened), vec![(b, Some(20)), (a, Some(10))]);
    assert_eq!(reopened.workspace().removed_projects().count(), 0);
}

#[test]
fn opening_a_project_saves_the_window_selection_with_it() {
    let directory = tempfile::tempdir().unwrap();
    let (a, b, _) = folders(directory.path());
    let mut store = Store::open(directory.path().join("state.json")).unwrap();
    let in_a = WorkingDirectory::new(a.path()).unwrap();
    store.create(Provider::Codex, &in_a, 1).unwrap();
    let second = store.open_window(Some(&a), Some(1), None).unwrap().value;
    store.open_project(second, &b, Some(&a), None, 5).unwrap();
    assert_eq!(store.workspace().selected_in(second), None);
    assert_eq!(store.workspace().selected_in(WindowId::FIRST), Some(1));
    assert_eq!(
        store.workspace().preferences().window_project(second),
        Some(b.clone())
    );
    let writes = store.writes();
    assert!(store.open_project(second, &a, None, Some(9), 6).is_err());
    assert!(store.open_project(WindowId(7), &a, None, None, 6).is_err());
    assert_eq!(store.writes(), writes, "a refused open writes nothing");
    store
        .open_project(second, &a, Some(&b), Some(1), 7)
        .unwrap();
    assert_eq!(store.workspace().selected_in(second), Some(1));
}

#[test]
fn quit_lists_the_projects_windows_show_without_reordering_listed_ones() {
    let directory = tempfile::tempdir().unwrap();
    let (a, b, c) = folders(directory.path());
    let mut store = Store::open(directory.path().join("state.json")).unwrap();
    store
        .open_project(WindowId::FIRST, &a, None, None, 50)
        .unwrap();
    let second = store.open_window(Some(&b), None, None).unwrap().value;
    let placement = Placement {
        x: 0.,
        y: 0.,
        width: 800.,
        height: 600.,
        maximized: false,
    };
    store
        .save_for_quit(
            vec![],
            vec![
                (WindowId::FIRST, placement, Some(a.clone())),
                (second, placement, Some(c.clone())),
            ],
            90,
        )
        .unwrap();
    assert_eq!(listed(&store), vec![(c, Some(90)), (a, Some(50))]);
}

#[test]
fn a_schema_2_database_gains_the_project_list() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    let (a, _, _) = folders(directory.path());
    {
        let mut store = Store::open(&path).unwrap();
        store
            .create(
                Provider::Codex,
                &WorkingDirectory::new(a.path()).unwrap(),
                7,
            )
            .unwrap();
    }
    let database = path.with_extension("sqlite3");
    rusqlite::Connection::open(&database)
        .unwrap()
        .execute_batch("DROP TABLE projects; DROP TABLE prompts; DROP TABLE deliveries; DROP TABLE notes; PRAGMA user_version=2;")
        .unwrap();
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.workspace().sessions().len(), 1);
    assert!(store.workspace().projects().is_empty());
    store
        .open_project(WindowId::FIRST, &a, None, None, 3)
        .unwrap();
    assert_eq!(listed(&Store::open(&path).unwrap()), vec![(a, Some(3))]);
}

#[test]
fn a_duplicate_project_row_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    drop(Store::open(&path).unwrap());
    rusqlite::Connection::open(path.with_extension("sqlite3"))
        .unwrap()
        .execute_batch(
            r#"INSERT INTO projects VALUES ('/x', 0, '{"path":"/x"}');
               INSERT INTO projects VALUES ('/y', 1, '{"path":"/x"}');"#,
        )
        .unwrap();
    let refused = Store::open(&path).err().unwrap();
    assert!(
        refused.to_string().contains("duplicate project"),
        "{refused}"
    );
}
