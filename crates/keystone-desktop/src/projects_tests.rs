use super::*;
use crate::windows::open;
use crate::{AppState, install_app_state, open_windows, request_quit};
use gpui::{AnyWindowHandle, TestAppContext, VisualTestContext};
use keystone_core::{Provider, Store, WindowId, WorkingDirectory};

struct Folders {
    _dir: tempfile::TempDir,
    root: PathBuf,
    alpha: ProjectId,
    beta: ProjectId,
}

/// Sessions 1 and 2 in alpha, 3 in beta, and whatever `seed` saves, as a relaunch
/// would find it. Alpha is the first window's project, showing session 1.
fn seeded(seed: impl FnOnce(&mut Store, &Folders)) -> (Folders, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let make = |name: &str| {
        std::fs::create_dir(root.join(name)).unwrap();
        ProjectId::new(&root.join(name)).unwrap()
    };
    let folders = Folders {
        alpha: make("alpha"),
        beta: make("beta"),
        root: root.clone(),
        _dir: dir,
    };
    let path = root.join("workspace.json");
    let mut store = Store::open(&path).unwrap();
    for (n, project) in [&folders.alpha, &folders.alpha, &folders.beta]
        .into_iter()
        .enumerate()
    {
        let directory = WorkingDirectory::new(project.path()).unwrap();
        store
            .create(Provider::Codex, &directory, n as u64 + 1)
            .unwrap();
    }
    store
        .open_project(WindowId::FIRST, &folders.alpha, None, Some(1), 10)
        .unwrap();
    seed(&mut store, &folders);
    (folders, path)
}

fn launch(path: &Path, cx: &mut TestAppContext) -> Entity<AppState> {
    let store = Store::open(path).unwrap();
    cx.executor().allow_parking();
    let state = cx.update(|cx| {
        keystone_ui::init(cx);
        crate::bind_keys(cx);
        let state = install_app_state(store, cx);
        for (id, placement) in state.read(cx).store().workspace().preferences().windows() {
            open(state.clone(), id, placement, true, false, cx).unwrap();
        }
        state
    });
    cx.run_until_parked();
    state
}

fn first(cx: &mut TestAppContext) -> (Entity<Keystone>, AnyWindowHandle) {
    let view = cx.update(|cx| open_windows(cx))[0].clone();
    let handle = cx.read(|cx| view.read(cx).handle);
    (view, handle)
}

fn visual<'a>(handle: AnyWindowHandle, cx: &'a mut TestAppContext) -> &'a mut VisualTestContext {
    let cx = VisualTestContext::from_window(handle, cx).into_mut();
    cx.simulate_resize(gpui::size(px(1280.), px(800.)));
    cx.run_until_parked();
    cx
}

fn project(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> ProjectId {
    view.read_with(cx, |this, _| this.presentation.project.clone().unwrap())
}

fn selected(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> Option<u64> {
    view.read_with(cx, |this, cx| this.selected(cx))
}

fn rows(cx: &mut VisualTestContext) -> Vec<u64> {
    (1..=9)
        .filter(|id| cx.debug_bounds(format!("session-{id}").leak()).is_some())
        .collect()
}

fn listed(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> Vec<(String, String, bool)> {
    view.read_with(cx, |this, cx| {
        this.projects
            .as_ref()
            .expect("the project list is open")
            .state
            .read(cx)
            .matches()
            .map(|item| {
                (
                    item.label.to_string(),
                    item.detail.clone().unwrap_or_default().to_string(),
                    item.removable,
                )
            })
            .collect()
    })
}

fn saved_projects(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> Vec<ProjectId> {
    view.read_with(cx, |this, cx| {
        let workspace = this.state.read(cx).store().workspace();
        workspace.projects().into_iter().map(|p| p.id()).collect()
    })
}

fn switch_by_keyboard(query: &str, cx: &mut VisualTestContext) {
    cx.simulate_keystrokes("ctrl-o");
    assert!(
        cx.debug_bounds("projects").is_some(),
        "Ctrl+O opens the list"
    );
    cx.simulate_input(query);
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
}

#[gpui::test]
fn ctrl_o_switches_the_window_to_another_project_and_back(cx: &mut TestAppContext) {
    let (folders, path) = seeded(|store, folders| {
        store
            .open_project(
                WindowId::FIRST,
                &folders.beta,
                Some(&folders.alpha),
                Some(3),
                20,
            )
            .unwrap();
        store
            .open_project(
                WindowId::FIRST,
                &folders.alpha,
                Some(&folders.beta),
                Some(1),
                30,
            )
            .unwrap();
    });
    launch(&path, cx);
    let (view, handle) = first(cx);
    let cx = visual(handle, cx);
    assert_eq!(project(&view, cx), folders.alpha);
    assert_eq!(rows(cx), vec![1, 2], "only alpha's sessions are listed");
    view.update(cx, |this, cx| this.select(2, cx));

    switch_by_keyboard("beta", cx);
    assert!(
        cx.debug_bounds("projects").is_none(),
        "Enter closes the list"
    );
    assert_eq!(project(&view, cx), folders.beta);
    assert_eq!(rows(cx), vec![3]);
    assert_eq!(selected(&view, cx), Some(3), "beta's last session shows");

    switch_by_keyboard("alpha", cx);
    assert_eq!(project(&view, cx), folders.alpha);
    assert_eq!(rows(cx), vec![1, 2]);
    assert_eq!(
        selected(&view, cx),
        Some(2),
        "the session it left shows again"
    );
    let saved = Store::open(&path).unwrap();
    let workspace = saved.workspace();
    assert_eq!(
        workspace.preferences().window_project(WindowId::FIRST),
        Some(folders.alpha.clone())
    );
    assert_eq!(workspace.selected_in(WindowId::FIRST), Some(2));
    assert_eq!(
        workspace
            .projects()
            .iter()
            .map(|p| p.id())
            .collect::<Vec<_>>(),
        vec![folders.alpha, folders.beta]
    );
}

#[gpui::test]
fn the_sidebar_header_opens_the_list_with_the_current_project_first(cx: &mut TestAppContext) {
    let (folders, path) = seeded(|store, folders| {
        store
            .open_project(
                WindowId::FIRST,
                &folders.beta,
                Some(&folders.alpha),
                None,
                20,
            )
            .unwrap();
        store
            .open_project(WindowId::FIRST, &folders.alpha, None, Some(1), 30)
            .unwrap();
    });
    launch(&path, cx);
    let (view, handle) = first(cx);
    let cx = visual(handle, cx);
    let header = cx.debug_bounds("project-switcher").unwrap().center();
    cx.simulate_click(header, gpui::Modifiers::none());
    cx.run_until_parked();
    let items = listed(&view, cx);
    assert_eq!(items.len(), 3, "{items:?}");
    assert_eq!(items[0].0, "alpha");
    assert!(items[0].1.starts_with("Current · "), "{items:?}");
    assert!(!items[0].2, "the current project cannot be removed");
    assert_eq!(items[1].0, "beta");
    assert!(items[1].2, "{items:?}");
    assert_eq!(items[2].0, "Open folder…");
    let row = cx.debug_bounds("projects-row-1").unwrap().center();
    cx.simulate_click(row, gpui::Modifiers::none());
    cx.run_until_parked();
    assert_eq!(project(&view, cx), folders.beta);
    assert_eq!(selected(&view, cx), Some(3));
}

#[gpui::test]
fn each_window_keeps_its_own_project(cx: &mut TestAppContext) {
    let (folders, path) = seeded(|_, _| {});
    launch(&path, cx);
    let (view, handle) = first(cx);
    handle
        .update(cx, |_, window, cx| {
            view.update(cx, |this, cx| this.open_new_window(None, window, cx))
        })
        .unwrap();
    cx.run_until_parked();
    let second = cx.update(|cx| open_windows(cx))[1].clone();
    let cx = visual(handle, cx);
    second.update(cx, |this, cx| this.open_project(folders.beta.clone(), cx));
    assert_eq!(project(&second, cx), folders.beta);
    assert_eq!(selected(&second, cx), Some(3));
    assert_eq!(project(&view, cx), folders.alpha, "the other window stays");
    assert_eq!(selected(&view, cx), Some(1));
    view.update(cx, |this, cx| {
        this.create(Provider::Claude, None, cx);
        let created = this.selected(cx).unwrap();
        assert_eq!(created, 4);
        let SessionContext::Recorded { directory, .. } =
            this.state.read(cx).session(created).unwrap().context()
        else {
            panic!("a new session records its folder")
        };
        assert_eq!(directory.path(), folders.alpha.path());
    });
    second.update(cx, |this, cx| {
        this.create(Provider::Codex, None, cx);
        let created = this.selected(cx).unwrap();
        let SessionContext::Recorded { directory, .. } =
            this.state.read(cx).session(created).unwrap().context()
        else {
            panic!("a new session records its folder")
        };
        assert_eq!(directory.path(), folders.beta.path(), "it starts in beta");
    });
    cx.update(|_, cx| request_quit(cx));
    cx.run_until_parked();
    let saved = Store::open(&path).unwrap();
    let prefs = saved.workspace().preferences();
    assert_eq!(prefs.window_project(WindowId::FIRST), Some(folders.alpha));
    assert_eq!(prefs.window_project(WindowId(1)), Some(folders.beta));
}

#[gpui::test]
fn a_relaunch_shows_the_project_and_session_the_window_showed(cx: &mut TestAppContext) {
    let (folders, path) = seeded(|store, folders| {
        store
            .open_project(
                WindowId::FIRST,
                &folders.beta,
                Some(&folders.alpha),
                Some(3),
                20,
            )
            .unwrap();
    });
    launch(&path, cx);
    let (view, handle) = first(cx);
    let cx = visual(handle, cx);
    assert_eq!(project(&view, cx), folders.beta);
    assert_eq!(selected(&view, cx), Some(3));
    assert_eq!(rows(cx), vec![3]);
    assert_eq!(saved_projects(&view, cx), vec![folders.beta, folders.alpha]);
}

#[gpui::test]
fn a_typed_path_opens_a_folder_and_a_wrong_one_opens_nothing(cx: &mut TestAppContext) {
    let (folders, path) = seeded(|_, _| {});
    let gamma = folders.root.join("gamma");
    std::fs::create_dir(&gamma).unwrap();
    launch(&path, cx);
    let (view, handle) = first(cx);
    let cx = visual(handle, cx);
    let missing = folders.root.join("nowhere");
    switch_by_keyboard(missing.to_str().unwrap(), cx);
    assert_eq!(project(&view, cx), folders.alpha);
    assert!(cx.debug_bounds("projects").is_some(), "the list stays open");
    assert!(
        view.read_with(cx, |this, _| this.error.clone())
            .unwrap()
            .starts_with("Project not opened.")
    );
    cx.simulate_keystrokes("escape");
    switch_by_keyboard(gamma.to_str().unwrap(), cx);
    assert_eq!(project(&view, cx).path(), gamma);
    assert_eq!(rows(cx), Vec::<u64>::new());
    assert_eq!(selected(&view, cx), None, "a new project opens empty");
    assert_eq!(
        saved_projects(&view, cx),
        vec![ProjectId::new(&gamma).unwrap(), folders.alpha]
    );
}

#[gpui::test]
fn removing_a_project_from_the_list_keeps_its_folder_and_sessions(cx: &mut TestAppContext) {
    let (folders, path) = seeded(|store, folders| {
        store
            .open_project(
                WindowId::FIRST,
                &folders.beta,
                Some(&folders.alpha),
                None,
                20,
            )
            .unwrap();
        store
            .open_project(WindowId::FIRST, &folders.alpha, None, Some(1), 30)
            .unwrap();
    });
    std::fs::write(folders.beta.path().join("notes.txt"), "kept").unwrap();
    launch(&path, cx);
    let (view, handle) = first(cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("ctrl-o");
    cx.simulate_keystrokes("shift-delete");
    assert_eq!(listed(&view, cx).len(), 3, "the current project stays");
    cx.simulate_keystrokes("down");
    assert!(cx.debug_bounds("projects-remove").is_some());
    cx.simulate_keystrokes("shift-delete");
    cx.run_until_parked();
    let names: Vec<String> = listed(&view, cx).into_iter().map(|i| i.0).collect();
    assert_eq!(names, vec!["alpha", "Open folder…"]);
    assert_eq!(saved_projects(&view, cx), vec![folders.alpha.clone()]);
    assert_eq!(
        std::fs::read_to_string(folders.beta.path().join("notes.txt")).unwrap(),
        "kept"
    );
    let saved = Store::open(&path).unwrap();
    assert_eq!(saved.workspace().sessions().len(), 3, "its sessions stay");
}

#[gpui::test]
fn a_project_whose_folder_is_missing_stays_listed_and_readable(cx: &mut TestAppContext) {
    let (folders, path) = seeded(|store, folders| {
        store
            .open_project(
                WindowId::FIRST,
                &folders.beta,
                Some(&folders.alpha),
                None,
                20,
            )
            .unwrap();
        store
            .open_project(WindowId::FIRST, &folders.alpha, None, Some(1), 30)
            .unwrap();
    });
    std::fs::remove_dir(folders.beta.path()).unwrap();
    launch(&path, cx);
    let (view, handle) = first(cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("ctrl-o");
    let items = listed(&view, cx);
    assert!(items[1].1.starts_with("Folder missing · "), "{items:?}");
    cx.simulate_keystrokes("down enter");
    cx.run_until_parked();
    assert_eq!(project(&view, cx), folders.beta);
    assert_eq!(rows(cx), vec![3], "its sessions stay readable");
    assert_eq!(selected(&view, cx), Some(3));
    view.update(cx, |this, cx| this.create(Provider::Claude, None, cx));
    view.read_with(cx, |this, cx| {
        assert_eq!(this.state.read(cx).store().workspace().sessions().len(), 3);
        assert!(
            this.error.is_some(),
            "no session starts in a missing folder"
        );
    });
    assert_eq!(
        saved_projects(&view, cx),
        vec![folders.beta, folders.alpha],
        "nothing is removed"
    );
}

#[gpui::test]
fn folders_of_sessions_from_before_projects_are_offered_too(cx: &mut TestAppContext) {
    let (folders, path) = seeded(|_, _| {});
    let gamma = folders.root.join("gamma");
    std::fs::create_dir(&gamma).unwrap();
    {
        let mut store = Store::open(&path).unwrap();
        store
            .create(
                Provider::Claude,
                &WorkingDirectory::new(&gamma).unwrap(),
                50,
            )
            .unwrap();
        store.select(1).unwrap();
    }
    launch(&path, cx);
    let (view, handle) = first(cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("ctrl-o");
    let items = listed(&view, cx);
    let names: Vec<&str> = items.iter().map(|i| i.0.as_str()).collect();
    assert_eq!(names, vec!["alpha", "gamma", "beta", "Open folder…"]);
    assert!(items[1].1.ends_with("gamma"), "{items:?}");
    assert!(
        !items[1].2,
        "a folder not on the list has nothing to remove"
    );
    cx.simulate_keystrokes("down enter");
    cx.run_until_parked();
    assert_eq!(project(&view, cx).path(), gamma);
    assert_eq!(selected(&view, cx), Some(4));
    assert_eq!(
        saved_projects(&view, cx),
        vec![ProjectId::new(&gamma).unwrap(), folders.alpha],
        "opening it lists it"
    );
}

#[gpui::test]
fn a_launch_saves_the_project_it_opens_on_before_any_quit(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("beta")).unwrap();
    let beta = ProjectId::new(&root.join("beta")).unwrap();
    let path = root.join("workspace.json");
    let mut store = Store::open(&path).unwrap();
    let directory = WorkingDirectory::new(beta.path()).unwrap();
    store.create(Provider::Codex, &directory, 1).unwrap();
    store.select(1).unwrap();
    drop(store);
    launch(&path, cx);
    let (view, handle) = first(cx);
    let cx = visual(handle, cx);
    assert_eq!(
        project(&view, cx),
        beta,
        "a window that never showed a project opens on its selected session's folder"
    );
    assert_eq!(selected(&view, cx), Some(1));
    let on_disk = Store::open(&path).unwrap();
    let workspace = on_disk.workspace();
    assert_eq!(
        workspace.preferences().window_project(WindowId::FIRST),
        Some(beta.clone())
    );
    assert_eq!(
        workspace
            .projects()
            .iter()
            .map(|p| p.id())
            .collect::<Vec<_>>(),
        vec![beta]
    );
    assert_eq!(workspace.selected_in(WindowId::FIRST), Some(1));
}

#[gpui::test]
fn saved_windows_from_a_newer_version_still_open_and_switch_projects(cx: &mut TestAppContext) {
    let (folders, path) = seeded(|_, _| {});
    let mut value = Store::open(&path).unwrap().saved_snapshot().unwrap();
    value["workspace"]["preferences"]["windows"] = serde_json::json!({"from": "the future"});
    let json = path.with_file_name("future.json");
    std::fs::write(&json, serde_json::to_vec(&value).unwrap()).unwrap();
    launch(&json, cx);
    let (view, handle) = first(cx);
    let cx = visual(handle, cx);
    let error = view.read_with(cx, |this, _| this.error.clone());
    assert_eq!(error, None);
    switch_by_keyboard("beta", cx);
    assert_eq!(project(&view, cx), folders.beta);
    assert_eq!(rows(cx), vec![3]);
    let error = view.read_with(cx, |this, _| this.error.clone());
    assert_eq!(error, None);
}

#[gpui::test]
fn a_launch_selects_one_of_the_projects_sessions(cx: &mut TestAppContext) {
    let (_folders, path) = seeded(|store, _| {
        store.select(3).unwrap();
    });
    launch(&path, cx);
    let (view, handle) = first(cx);
    let cx = visual(handle, cx);
    let shown = selected(&view, cx).expect("an alpha session is selected");
    assert!(
        rows(cx).contains(&shown),
        "selected {shown} is in the sidebar"
    );
    let saved = view.read_with(cx, |this, cx| {
        this.state
            .read(cx)
            .store()
            .workspace()
            .selected_in(WindowId::FIRST)
    });
    assert_eq!(
        saved,
        Some(shown),
        "the store selects what the window shows"
    );
}

#[gpui::test]
fn a_removed_project_with_sessions_stays_off_the_list(cx: &mut TestAppContext) {
    let (folders, path) = seeded(|store, folders| {
        store
            .open_project(
                WindowId::FIRST,
                &folders.beta,
                Some(&folders.alpha),
                None,
                20,
            )
            .unwrap();
        store
            .open_project(WindowId::FIRST, &folders.alpha, None, Some(1), 30)
            .unwrap();
    });
    launch(&path, cx);
    let (view, handle) = first(cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("ctrl-o");
    cx.simulate_keystrokes("down shift-delete");
    cx.run_until_parked();
    assert_eq!(saved_projects(&view, cx), vec![folders.alpha.clone()]);
    cx.simulate_keystrokes("escape");
    cx.simulate_keystrokes("ctrl-o");
    let items = listed(&view, cx);
    assert!(
        !items.iter().any(|i| i.0 == "beta"),
        "beta is back: {items:?}"
    );
    let sessions = view.read_with(cx, |this, cx| {
        this.state.read(cx).store().workspace().sessions().len()
    });
    assert_eq!(sessions, 3, "its session is kept");

    cx.simulate_keystrokes("escape");
    switch_by_keyboard(&folders.beta.path().display().to_string(), cx);
    assert_eq!(
        project(&view, cx),
        folders.beta,
        "typing its path reopens it"
    );
    assert_eq!(rows(cx), vec![3]);
    assert!(saved_projects(&view, cx).contains(&folders.beta));
}

#[gpui::test]
fn split_right_opens_a_session_of_the_windows_project(cx: &mut TestAppContext) {
    let (_folders, path) = seeded(|_, _| {});
    launch(&path, cx);
    let (view, handle) = first(cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("ctrl-\\");
    cx.run_until_parked();
    let tabs = view.read_with(cx, |this, cx| this.session_tabs(cx));
    assert!(
        tabs.view_of(3).is_none(),
        "beta's session stays out of alpha"
    );
    assert!(
        tabs.view_of(2).is_some(),
        "alpha's other session opens beside"
    );
}

#[gpui::test]
fn the_palette_opened_from_the_project_list_returns_focus_to_the_composer(cx: &mut TestAppContext) {
    let (_folders, path) = seeded(|_, _| {});
    launch(&path, cx);
    let (view, handle) = first(cx);
    let cx = visual(handle, cx);
    let composer = |view: &Entity<Keystone>, cx: &mut VisualTestContext| {
        cx.update(|window, cx| view.read(cx).composer.focus_handle(cx).is_focused(window))
    };
    cx.simulate_keystrokes("ctrl-l");
    assert!(composer(&view, cx));
    cx.simulate_keystrokes("ctrl-o");
    cx.simulate_keystrokes("ctrl-k");
    assert!(view.read_with(cx, |this, _| this.palette.is_some()
        && this.projects.is_none()));
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(composer(&view, cx), "focus returns to the composer");
}

#[gpui::test]
fn a_switch_the_store_refuses_leaves_the_window_on_its_project(cx: &mut TestAppContext) {
    let (folders, path) = seeded(|_, _| {});
    let state = launch(&path, cx);
    let (view, handle) = first(cx);
    let cx = visual(handle, cx);
    let database = state.read_with(cx, |state, _| {
        keystone_core::database_path(state.store().path())
    });
    let lock = rusqlite::Connection::open(database).unwrap();
    lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
    switch_by_keyboard("beta", cx);
    drop(lock);
    assert_eq!(project(&view, cx), folders.alpha);
    assert_eq!(selected(&view, cx), Some(1));
    assert_eq!(rows(cx), vec![1, 2]);
    let error = view.read_with(cx, |this, _| this.error.clone().unwrap_or_default());
    assert!(error.starts_with("Project not opened."), "{error}");
}

#[gpui::test]
fn a_tab_saved_with_another_projects_session_shows_this_projects(cx: &mut TestAppContext) {
    let (_folders, path) = seeded(|store, folders| {
        let composition = store
            .workspace()
            .preferences()
            .composition_in(&folders.alpha, WindowId::FIRST);
        let (view, _) = composition
            .views()
            .find(|(_, component)| *component == keystone_core::Component::Conversation)
            .unwrap();
        let mut tabs = keystone_core::SessionTabs::default();
        tabs.hold(view, 3);
        store
            .show_in(
                &folders.alpha,
                WindowId::FIRST,
                Some(composition),
                tabs,
                Some(1),
            )
            .unwrap();
    });
    launch(&path, cx);
    let (view, handle) = first(cx);
    let cx = visual(handle, cx);
    view.read_with(cx, |this, cx| {
        let active = this.active_view(cx).unwrap();
        assert!(this.session_tabs(cx).view_of(3).is_none());
        assert_eq!(this.view_session(active, cx), Some(1));
    });
    assert_eq!(rows(cx), vec![1, 2]);
}

#[gpui::test]
fn a_session_folder_inside_another_is_offered_as_the_outer_one(cx: &mut TestAppContext) {
    let (folders, path) = seeded(|_, _| {});
    let gamma = folders.root.join("gamma");
    let nested = gamma.join("crates/x");
    std::fs::create_dir_all(&nested).unwrap();
    {
        let mut store = Store::open(&path).unwrap();
        for (folder, at) in [(&gamma, 50), (&nested, 60)] {
            store
                .create(
                    Provider::Claude,
                    &WorkingDirectory::new(folder).unwrap(),
                    at,
                )
                .unwrap();
        }
        store.select(1).unwrap();
    }
    launch(&path, cx);
    let (view, handle) = first(cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("ctrl-o");
    let items = listed(&view, cx);
    let names: Vec<&str> = items.iter().map(|i| i.0.as_str()).collect();
    assert_eq!(names, vec!["alpha", "gamma", "beta", "Open folder…"]);
    cx.simulate_keystrokes("down enter");
    cx.run_until_parked();
    assert_eq!(project(&view, cx).path(), gamma);
    assert_eq!(
        rows(cx),
        vec![4, 5],
        "the nested folder's session is gamma's too"
    );
}
