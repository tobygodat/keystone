use super::*;
use crate::{Axis, Edit, Place, Provider, Size, WorkingDirectory, database_path};
use std::path::Path;

fn working_directory() -> WorkingDirectory {
    WorkingDirectory::new(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap()
}

/// A store at `directory` with three sessions, the last one selected.
fn three_sessions(directory: &Path) -> (Store, ProjectId) {
    let mut store = Store::open(directory.join("state.json")).unwrap();
    for provider in [Provider::Codex, Provider::Claude, Provider::Codex] {
        store.create(provider, &working_directory(), 1).unwrap();
    }
    (store, ProjectId::new(directory).unwrap())
}

fn split() -> Composition {
    Composition::default()
        .edit(Edit::Open {
            view: ViewId(3),
            component: Component::Conversation,
            at: Place::Beside {
                target: ViewId::MAIN,
                axis: Axis::Row,
                after: true,
            },
            size: Size::Fill(std::num::NonZeroU16::MIN),
        })
        .unwrap()
}

fn placement(x: f32) -> Placement {
    Placement {
        x,
        y: 20.,
        width: 900.,
        height: 600.,
        maximized: false,
    }
}

fn saved_windows(directory: &Path) -> Option<Value> {
    Store::open(directory.join("state.json"))
        .unwrap()
        .saved_snapshot()
        .unwrap()["workspace"]["preferences"]
        .get("windows")
        .cloned()
}

#[test]
fn a_new_window_saves_its_own_selection_layout_and_tabs_in_one_write_each() {
    let directory = tempfile::tempdir().unwrap();
    let (mut store, project) = three_sessions(directory.path());
    let writes = store.writes();
    let second = store
        .open_window(None, Some(2), Some(placement(40.)))
        .unwrap()
        .value;
    assert_eq!(second, WindowId(1));
    assert_eq!(store.writes(), writes + 1);
    assert!(store.open_window(None, Some(99), None).is_err());
    assert_eq!(store.writes(), writes + 1);

    let mut tabs = SessionTabs::default();
    tabs.hold(ViewId::MAIN, 1);
    tabs.hold(ViewId(3), 2);
    store
        .show_in(&project, second, Some(split()), tabs.clone(), Some(2))
        .unwrap();
    assert_eq!(store.writes(), writes + 2);
    store.select_in(second, 1).unwrap();
    store.select_in(second, 1).unwrap();
    assert_eq!(store.writes(), writes + 3);

    let reopened = Store::open(directory.path().join("state.json")).unwrap();
    let workspace = reopened.workspace();
    let preferences = workspace.preferences();
    assert_eq!(
        preferences.windows(),
        [(WindowId::FIRST, None), (second, Some(placement(40.)))]
    );
    assert_eq!(workspace.selected_in(second), Some(1));
    assert_eq!(preferences.composition_in(&project, second), split());
    assert_eq!(preferences.session_tabs_in(&project, second), tabs);
    assert_eq!(preferences.held_in(&project, second, ViewId(3)), Some(2));

    assert_eq!(
        workspace.selected(),
        Some(3),
        "the first window keeps its own"
    );
    assert_eq!(workspace.selected_in(WindowId::FIRST), Some(3));
    assert_eq!(preferences.composition(&project), Composition::default());
    assert!(preferences.session_tabs(&project).is_empty());
    assert_eq!(
        preferences.composition_in(&project, WindowId(7)),
        Composition::default(),
        "an unknown window draws the default"
    );
    assert_eq!(workspace.selected_in(WindowId(7)), None);
}

#[test]
fn a_session_created_in_a_window_is_selected_there_only() {
    let directory = tempfile::tempdir().unwrap();
    let (mut store, project) = three_sessions(directory.path());
    let second = store.open_window(None, Some(1), None).unwrap().value;
    let writes = store.writes();
    let mut tabs = SessionTabs::default();
    tabs.hold(ViewId::MAIN, 1);
    let created = store
        .create_showing(
            Provider::Claude,
            &working_directory(),
            5,
            second,
            Some((&project, split(), tabs, ViewId(3))),
        )
        .unwrap()
        .value;
    assert_eq!(store.writes(), writes + 1);
    let workspace = store.workspace();
    assert_eq!(workspace.selected_in(second), Some(created));
    assert_eq!(workspace.selected(), Some(3));
    assert_eq!(
        workspace
            .preferences()
            .session_tabs_in(&project, second)
            .iter()
            .collect::<Vec<_>>(),
        [(ViewId::MAIN, 1), (ViewId(3), created)]
    );
    assert!(workspace.preferences().session_tabs(&project).is_empty());
    assert!(
        store
            .create_showing(Provider::Claude, &working_directory(), 6, WindowId(9), None)
            .is_err()
    );
    assert_eq!(store.writes(), writes + 1);
}

#[test]
fn a_layout_saved_in_one_window_forgets_only_that_windows_closed_tabs() {
    let directory = tempfile::tempdir().unwrap();
    let (mut store, project) = three_sessions(directory.path());
    let second = store.open_window(None, None, None).unwrap().value;
    let mut tabs = SessionTabs::default();
    tabs.hold(ViewId::MAIN, 1);
    tabs.hold(ViewId(3), 2);
    store
        .show_in(&project, second, Some(split()), tabs.clone(), Some(2))
        .unwrap();
    store
        .show(&project, Some(split()), tabs.clone(), Some(2))
        .unwrap();
    store
        .set_layout_in(&project, second, Some(Composition::default().into()))
        .unwrap();
    let preferences = store.workspace().preferences();
    assert_eq!(
        preferences
            .session_tabs_in(&project, second)
            .iter()
            .collect::<Vec<_>>(),
        [(ViewId::MAIN, 1)]
    );
    assert_eq!(preferences.session_tabs(&project), tabs);
    assert_eq!(preferences.composition(&project), split());
}

#[test]
fn closing_a_window_forgets_it_and_the_next_one_takes_the_first_windows_place() {
    let directory = tempfile::tempdir().unwrap();
    let (mut store, project) = three_sessions(directory.path());
    store
        .set_placement(WindowId::FIRST, placement(0.), None)
        .unwrap();
    let second = store
        .open_window(None, Some(1), Some(placement(40.)))
        .unwrap()
        .value;
    let third = store
        .open_window(None, Some(2), Some(placement(80.)))
        .unwrap()
        .value;
    let mut tabs = SessionTabs::default();
    tabs.hold(ViewId(3), 1);
    store
        .show_in(&project, second, Some(split()), tabs.clone(), Some(1))
        .unwrap();

    let writes = store.writes();
    assert_eq!(store.close_window(third).unwrap().value, None);
    assert_eq!(store.writes(), writes + 1);
    assert!(store.close_window(third).is_err());
    assert_eq!(store.writes(), writes + 1);
    for id in 1..=3 {
        assert!(store.workspace().sessions().iter().any(|s| s.id() == id));
    }

    assert_eq!(
        store.close_window(WindowId::FIRST).unwrap().value,
        Some(second)
    );
    assert_eq!(store.writes(), writes + 2);
    let reopened = Store::open(directory.path().join("state.json")).unwrap();
    let workspace = reopened.workspace();
    let preferences = workspace.preferences();
    assert_eq!(
        preferences.windows(),
        [(WindowId::FIRST, Some(placement(40.)))]
    );
    assert_eq!(workspace.selected(), Some(1));
    assert_eq!(preferences.composition(&project), split());
    assert_eq!(preferences.session_tabs(&project), tabs);

    let mut reopened = reopened;
    assert!(reopened.close_window(WindowId::FIRST).is_err());
}

#[test]
fn quit_saves_every_draft_and_placement_in_one_write() {
    let directory = tempfile::tempdir().unwrap();
    let (mut store, project) = three_sessions(directory.path());
    let second = store.open_window(None, Some(1), None).unwrap().value;
    let writes = store.writes();
    store
        .save_for_quit(
            vec![(1, "one".into()), (3, "three".into())],
            vec![
                (WindowId::FIRST, placement(5.), Some(project.clone())),
                (second, placement(60.), None),
            ],
        )
        .unwrap();
    assert_eq!(store.writes(), writes + 1);
    store
        .save_for_quit(
            vec![(1, "one".into())],
            vec![(second, placement(60.), None)],
        )
        .unwrap();
    assert_eq!(store.writes(), writes + 1, "nothing changed");
    assert!(store.save_for_quit(vec![(9, "x".into())], vec![]).is_err());
    assert_eq!(store.writes(), writes + 1);

    let reopened = Store::open(directory.path().join("state.json")).unwrap();
    let drafts: Vec<_> = reopened
        .workspace()
        .sessions()
        .iter()
        .map(|s| s.draft().to_owned())
        .collect();
    assert_eq!(drafts, ["one", "", "three"]);
    assert_eq!(
        reopened.workspace().preferences().windows(),
        [
            (WindowId::FIRST, Some(placement(5.))),
            (second, Some(placement(60.)))
        ]
    );
    assert_eq!(
        reopened
            .workspace()
            .preferences()
            .window_project(WindowId::FIRST),
        Some(project)
    );
}

#[test]
fn a_state_from_before_windows_opens_one_window_and_unreadable_windows_are_kept() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    std::fs::write(
        &path,
        r#"{"version":1,"next_id":2,"workspace":{"sessions":[{"id":1,"title":"Old","provider":"Codex","settled":false,"draft":"keep"}],"selected":1}}"#,
    )
    .unwrap();
    let mut store = Store::open(&path).unwrap();
    assert_eq!(
        store.workspace().preferences().windows(),
        [(WindowId::FIRST, None)]
    );
    store.set_draft(1, "edited".into()).unwrap();
    assert_eq!(saved_windows(directory.path()), None);

    std::fs::remove_file(database_path(&path)).unwrap();
    std::fs::write(
        &path,
        r#"{"version":1,"next_id":2,"workspace":{"sessions":[{"id":1,"title":"Old","provider":"Codex","settled":false,"draft":"keep"}],"selected":1,"preferences":{"windows":[{"id":"later"}]}}}"#,
    )
    .unwrap();
    let mut store = Store::open(&path).unwrap();
    assert_eq!(
        store.workspace().preferences().windows(),
        [(WindowId::FIRST, None)]
    );
    let writes = store.writes();
    assert!(store.open_window(None, None, None).is_err());
    assert!(
        store
            .set_placement(WindowId::FIRST, placement(0.), None)
            .is_err()
    );
    assert_eq!(store.writes(), writes);
    store.select(1).unwrap();
    store
        .save_for_quit(
            vec![(1, "quit draft".into())],
            vec![(WindowId::FIRST, placement(0.), None)],
        )
        .unwrap();
    assert_eq!(store.workspace().sessions()[0].draft(), "quit draft");
    assert_eq!(
        saved_windows(directory.path()),
        Some(serde_json::json!([{"id": "later"}]))
    );

    std::fs::remove_file(database_path(&path)).unwrap();
    std::fs::write(
        &path,
        r#"{"version":1,"next_id":2,"workspace":{"sessions":[{"id":1,"title":"Old","provider":"Codex","settled":false,"draft":"keep"}],"selected":1,"preferences":{"windows":[{"id":0},{"id":4,"selected":1,"pinned":true}]}}}"#,
    )
    .unwrap();
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.workspace().selected_in(WindowId(4)), Some(1));
    assert_eq!(
        store.open_window(None, None, None).unwrap().value,
        WindowId(5)
    );
    assert_eq!(
        saved_windows(directory.path()),
        Some(serde_json::json!([{"id": 0}, {"id": 4, "selected": 1, "pinned": true}, {"id": 5}]))
    );
}

#[test]
fn each_window_saves_the_project_it_shows_and_the_heir_keeps_its_own() {
    let directory = tempfile::tempdir().unwrap();
    let (mut store, project) = three_sessions(directory.path());
    let other_dir = directory.path().join("other");
    std::fs::create_dir(&other_dir).unwrap();
    let other = ProjectId::new(&other_dir).unwrap();
    assert_eq!(
        store
            .workspace()
            .preferences()
            .window_project(WindowId::FIRST),
        None
    );
    store.set_window_project(WindowId::FIRST, &project).unwrap();
    let second = store
        .open_window(Some(&other), Some(2), None)
        .unwrap()
        .value;
    let writes = store.writes();
    store.set_window_project(second, &other).unwrap();
    assert_eq!(store.writes(), writes, "unchanged");
    assert!(store.set_window_project(WindowId(9), &other).is_err());

    let reopened = Store::open(directory.path().join("state.json")).unwrap();
    let preferences = reopened.workspace().preferences();
    assert_eq!(
        preferences.window_project(WindowId::FIRST),
        Some(project.clone())
    );
    assert_eq!(preferences.window_project(second), Some(other.clone()));

    let mut reopened = reopened;
    reopened.close_window(WindowId::FIRST).unwrap();
    assert_eq!(
        reopened
            .workspace()
            .preferences()
            .window_project(WindowId::FIRST),
        Some(other)
    );
}

#[test]
fn an_agent_started_session_is_selected_in_no_window_and_each_window_lists_its_own_layouts() {
    let directory = tempfile::tempdir().unwrap();
    let (mut store, project) = three_sessions(directory.path());
    let second = store.open_window(None, None, None).unwrap().value;
    let writes = store.writes();
    let child = store
        .create_unselected(Provider::Codex, &working_directory(), 9)
        .unwrap()
        .value;
    assert_eq!(store.writes(), writes + 1);
    let workspace = store.workspace();
    assert!(workspace.sessions().iter().any(|s| s.id() == child));
    assert_eq!(workspace.selected_in(WindowId::FIRST), Some(3));
    assert_eq!(workspace.selected_in(second), None);

    store
        .set_layout_in(&project, second, Some(split().into()))
        .unwrap();
    let preferences = store.workspace().preferences();
    assert_eq!(preferences.layouts_in(second).count(), 1);
    assert_eq!(preferences.layouts_in(WindowId::FIRST).count(), 0);
    assert_eq!(preferences.project_layouts().count(), 0);
}

#[test]
fn closing_the_first_window_keeps_its_layouts_for_projects_the_heir_never_laid_out() {
    let directory = tempfile::tempdir().unwrap();
    let (mut store, project) = three_sessions(directory.path());
    let other_dir = directory.path().join("other");
    std::fs::create_dir(&other_dir).unwrap();
    let other = ProjectId::new(&other_dir).unwrap();
    let mut tabs = SessionTabs::default();
    tabs.hold(ViewId(3), 1);
    store
        .show(&other, Some(split()), tabs.clone(), Some(1))
        .unwrap();
    store
        .show(&project, Some(split()), tabs.clone(), Some(1))
        .unwrap();
    let second = store.open_window(None, Some(2), None).unwrap().value;
    let mut heirs = SessionTabs::default();
    heirs.hold(ViewId::MAIN, 2);
    store
        .show_in(
            &project,
            second,
            Some(Composition::default()),
            heirs.clone(),
            Some(2),
        )
        .unwrap();
    store.close_window(WindowId::FIRST).unwrap();
    let preferences = store.workspace().preferences();
    assert_eq!(preferences.composition(&project), Composition::default());
    assert_eq!(preferences.session_tabs(&project), heirs);
    assert_eq!(preferences.composition(&other), split(), "kept");
    assert_eq!(preferences.session_tabs(&other), tabs);
}

#[test]
fn closing_the_first_window_drops_its_layout_where_the_heir_kept_the_default() {
    let directory = tempfile::tempdir().unwrap();
    let (mut store, project) = three_sessions(directory.path());
    let mut tabs = SessionTabs::default();
    tabs.hold(ViewId(3), 1);
    store
        .show(&project, Some(split()), tabs.clone(), Some(1))
        .unwrap();
    let second = store
        .open_window(Some(&project), Some(2), None)
        .unwrap()
        .value;
    store.close_window(WindowId::FIRST).unwrap();
    let preferences = store.workspace().preferences();
    assert_eq!(preferences.composition(&project), Composition::default());
    assert_eq!(preferences.session_tabs(&project), SessionTabs::default());
    assert_eq!(store.workspace().selected(), Some(2));
    assert_ne!(second, WindowId::FIRST);

    let other = directory.path().join("tabs");
    std::fs::create_dir(&other).unwrap();
    let (mut store, project) = three_sessions(&other);
    store
        .show(&project, Some(split()), tabs.clone(), Some(1))
        .unwrap();
    let second = store.open_window(None, Some(2), None).unwrap().value;
    let mut heirs = SessionTabs::default();
    heirs.hold(ViewId::MAIN, 2);
    store
        .show_in(&project, second, None, heirs.clone(), Some(2))
        .unwrap();
    store.close_window(WindowId::FIRST).unwrap();
    let preferences = store.workspace().preferences();
    assert_eq!(preferences.composition(&project), Composition::default());
    assert_eq!(preferences.session_tabs(&project), heirs);
}

#[test]
fn the_first_window_closes_when_its_saved_layouts_cannot_be_read() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    let (mut store, project) = three_sessions(directory.path());
    let second = store
        .open_window(Some(&project), Some(1), None)
        .unwrap()
        .value;
    store
        .show_in(
            &project,
            second,
            Some(split()),
            SessionTabs::default(),
            Some(1),
        )
        .unwrap();
    let mut saved = store.saved_snapshot().unwrap();
    drop(store);
    saved["workspace"]["preferences"]["project_layouts"] = "wide".into();
    saved["workspace"]["preferences"]["project_tabs"] = "wide".into();
    std::fs::remove_file(database_path(&path)).unwrap();
    std::fs::write(&path, saved.to_string()).unwrap();

    let mut store = Store::open(&path).unwrap();
    assert_eq!(
        store.close_window(WindowId::FIRST).unwrap().value,
        Some(second)
    );
    let preferences = store.workspace().preferences();
    assert_eq!(preferences.windows(), [(WindowId::FIRST, None)]);
    assert_eq!(preferences.composition(&project), split());
    assert_eq!(preferences.window_project(WindowId::FIRST), Some(project));
}

#[test]
fn closing_the_first_window_keeps_its_layouts_when_the_heirs_cannot_be_read() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    let (mut store, project) = three_sessions(directory.path());
    let other_dir = directory.path().join("other");
    std::fs::create_dir(&other_dir).unwrap();
    let other = ProjectId::new(&other_dir).unwrap();
    let mut tabs = SessionTabs::default();
    tabs.hold(ViewId(3), 1);
    for shown in [&project, &other] {
        store
            .show(shown, Some(split()), tabs.clone(), Some(1))
            .unwrap();
    }
    let second = store
        .open_window(Some(&project), Some(2), None)
        .unwrap()
        .value;
    let mut saved = store.saved_snapshot().unwrap();
    drop(store);
    let heir = saved["workspace"]["preferences"]["windows"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|w| w["id"] == second.0)
        .unwrap();
    heir["layouts"] = "wide".into();
    heir["tabs"] = "wide".into();
    std::fs::remove_file(database_path(&path)).unwrap();
    std::fs::write(&path, saved.to_string()).unwrap();

    let mut store = Store::open(&path).unwrap();
    assert_eq!(
        store.close_window(WindowId::FIRST).unwrap().value,
        Some(second)
    );
    let preferences = store.workspace().preferences();
    assert_eq!(preferences.composition(&other), split(), "kept");
    assert_eq!(preferences.session_tabs(&other), tabs);
    assert_eq!(preferences.composition(&project), Composition::default());
}

#[test]
fn a_tab_moved_to_another_window_leaves_the_first_in_the_same_write() {
    let directory = tempfile::tempdir().unwrap();
    let (mut store, project) = three_sessions(directory.path());
    let second = store.open_window(Some(&project), None, None).unwrap().value;
    let mut tabs = SessionTabs::default();
    tabs.hold(ViewId::MAIN, 1);
    tabs.hold(ViewId(3), 2);
    store
        .show_in(&project, WindowId::FIRST, Some(split()), tabs, Some(2))
        .unwrap();
    let writes = store.writes();

    let mut left = SessionTabs::default();
    left.hold(ViewId::MAIN, 1);
    let mut arrived = SessionTabs::default();
    arrived.hold(ViewId(3), 2);
    let from = WindowView {
        window: WindowId::FIRST,
        project: project.clone(),
        layout: Some(Composition::default()),
        tabs: left.clone(),
        selected: Some(1),
    };
    let to = WindowView {
        window: second,
        project: project.clone(),
        layout: Some(split()),
        tabs: arrived.clone(),
        selected: Some(2),
    };
    assert!(store.move_tab(from.clone(), from.clone()).is_err());
    assert!(
        store
            .move_tab(
                from.clone(),
                WindowView {
                    selected: Some(99),
                    ..to.clone()
                }
            )
            .is_err()
    );
    assert_eq!(store.writes(), writes, "a refused move writes nothing");
    store.move_tab(from, to).unwrap();
    assert_eq!(store.writes(), writes + 1);

    let reopened = Store::open(directory.path().join("state.json")).unwrap();
    let workspace = reopened.workspace();
    let preferences = workspace.preferences();
    assert_eq!(preferences.session_tabs_in(&project, WindowId::FIRST), left);
    assert_eq!(
        preferences.composition_in(&project, WindowId::FIRST),
        Composition::default()
    );
    assert_eq!(workspace.selected_in(WindowId::FIRST), Some(1));
    assert_eq!(preferences.session_tabs_in(&project, second), arrived);
    assert_eq!(preferences.composition_in(&project, second), split());
    assert_eq!(workspace.selected_in(second), Some(2));
}

#[test]
fn a_tab_moved_to_a_new_window_opens_it_in_the_same_write() {
    let directory = tempfile::tempdir().unwrap();
    let (mut store, project) = three_sessions(directory.path());
    let mut tabs = SessionTabs::default();
    tabs.hold(ViewId::MAIN, 1);
    tabs.hold(ViewId(3), 2);
    store
        .show_in(&project, WindowId::FIRST, Some(split()), tabs, Some(2))
        .unwrap();
    let writes = store.writes();
    let mut left = SessionTabs::default();
    left.hold(ViewId::MAIN, 1);
    let from = WindowView {
        window: WindowId::FIRST,
        project: project.clone(),
        layout: Some(Composition::default()),
        tabs: left.clone(),
        selected: Some(1),
    };

    assert!(
        store
            .move_tab_to_new_window(from.clone(), 99, None)
            .is_err()
    );
    assert_eq!(store.writes(), writes);
    let opened = store
        .move_tab_to_new_window(from, 2, Some(placement(60.)))
        .unwrap()
        .value;
    assert_eq!(store.writes(), writes + 1);

    let reopened = Store::open(directory.path().join("state.json")).unwrap();
    let workspace = reopened.workspace();
    let preferences = workspace.preferences();
    assert_eq!(
        preferences.windows(),
        [(WindowId::FIRST, None), (opened, Some(placement(60.)))]
    );
    assert_eq!(preferences.window_project(opened), Some(project.clone()));
    assert_eq!(workspace.selected_in(opened), Some(2));
    assert_eq!(preferences.session_tabs_in(&project, WindowId::FIRST), left);
    assert_eq!(workspace.selected_in(WindowId::FIRST), Some(1));
}
