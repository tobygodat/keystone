use super::*;
use crate::conversation::Row;
use crate::{Keystone, install_app_state, open_windows, request_quit};
use gpui::{TestAppContext, VisualTestContext, WindowHandle};
use keystone_agent::{ItemKind, ItemStatus};
use keystone_core::{Store, WindowId};

/// Launches Keystone on a new workspace as `main` does, with `seed` saved first.
fn launch(
    dir: &Path,
    seed: impl FnOnce(&mut Store),
    cx: &mut TestAppContext,
) -> (PathBuf, Entity<AppState>) {
    let path = dir.join("workspace.json");
    let mut store = Store::open(&path).unwrap();
    seed(&mut store);
    cx.executor().allow_parking();
    let state = cx.update(|cx| {
        keystone_ui::init(cx);
        crate::bind_keys(cx);
        let state = install_app_state(store, cx);
        for (id, placement) in state.read(cx).store().workspace().preferences().windows() {
            crate::windows::open(state.clone(), id, placement, true, false, cx).unwrap();
        }
        state
    });
    cx.run_until_parked();
    (path, state)
}

fn quick_window(cx: &mut TestAppContext) -> Option<WindowHandle<QuickComposer>> {
    cx.update(|cx| {
        let window = cx.global::<DesktopApp>().quick?;
        window.update(cx, |_, _, _| ()).ok().map(|_| window)
    })
}

fn open_quick(cx: &mut TestAppContext) -> VisualTestContext {
    cx.update(open);
    cx.run_until_parked();
    let window = quick_window(cx).expect("the quick composer opened");
    VisualTestContext::from_window(window.into(), cx)
}

fn text(window: WindowHandle<QuickComposer>, cx: &mut TestAppContext) -> String {
    window
        .read_with(cx, |view, cx| view.input.read(cx).value().to_owned())
        .unwrap()
}

fn first_view(cx: &mut TestAppContext) -> Entity<Keystone> {
    cx.update(|cx| open_windows(cx)).remove(0)
}

/// Asks on another thread, as `--quick` does, while the app runs.
fn ask_app(socket: &Path, cx: &mut TestAppContext) -> bool {
    let socket = socket.to_owned();
    let asked = std::thread::spawn(move || ask(&socket, Duration::from_secs(5)));
    while !asked.is_finished() {
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    asked.join().unwrap()
}

#[gpui::test]
fn a_request_on_the_socket_opens_one_quick_composer(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (_, state) = launch(dir.path(), |_| {}, cx);
    let socket = dir.path().join("quick.sock");
    assert!(!ask(&socket, Duration::from_secs(5)), "nothing listens yet");
    std::fs::write(&socket, "").unwrap();
    assert!(
        !ask(&socket, Duration::from_secs(5)),
        "a stale socket file answers nothing"
    );
    cx.update(|cx| listen(&socket, cx)).unwrap();
    assert!(ask_app(&socket, cx));
    assert_eq!(cx.windows().len(), 2, "a second window opened");
    assert!(quick_window(cx).is_some());
    assert!(ask_app(&socket, cx));
    assert_eq!(
        cx.windows().len(),
        2,
        "asking again brings the same one forward"
    );
    let mut stream = UnixStream::connect(&socket).unwrap();
    stream.write_all(b"other\n").unwrap();
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer).unwrap();
    assert_eq!(answer, "", "anything else is not answered");

    state.update(cx, |state, _| state.phase = Phase::Exiting);
    assert!(!ask_app(&socket, cx), "a quitting app answers no");
    state.update(cx, |state, _| state.phase = Phase::Running);
}

#[test]
fn the_socket_is_named_after_the_database_the_workspace_path_resolves_to() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, dir.path().join("alias")).unwrap();
    let named = socket(&real.join("workspace.json"));
    assert_eq!(named, socket(&real.join("workspace.sqlite3")));
    assert_eq!(named, socket(&dir.path().join("alias/workspace.json")));
    assert_ne!(named, socket(&dir.path().join("workspace.json")));
}

#[gpui::test]
fn ctrl_alt_space_opens_the_quick_composer_from_a_window(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    launch(dir.path(), |_| {}, cx);
    let view = first_view(cx);
    let window = cx.read(|cx| view.read(cx).handle);
    let vcx = &mut VisualTestContext::from_window(window, cx);
    vcx.simulate_keystrokes("secondary-alt-space");
    vcx.run_until_parked();
    assert!(quick_window(cx).is_some());
}

#[gpui::test]
fn escape_closes_and_keeps_the_text_for_the_next_opening_and_relaunch(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (path, state) = launch(dir.path(), |_| {}, cx);
    let vcx = &mut open_quick(cx);
    vcx.simulate_input("first line");
    vcx.simulate_keystrokes("shift-enter");
    vcx.simulate_input("second");
    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();
    assert!(quick_window(cx).is_none(), "Escape closed it");
    assert_eq!(cx.windows().len(), 1);
    let sessions = cx.read(|cx| state.read(cx).store().workspace().sessions().len());
    assert_eq!(sessions, 0, "nothing was created");
    assert_eq!(
        Store::open(&path)
            .unwrap()
            .workspace()
            .preferences()
            .quick()
            .text,
        "first line\nsecond"
    );
    open_quick(cx);
    let window = quick_window(cx).unwrap();
    assert_eq!(text(window, cx), "first line\nsecond");

    let vcx = &mut VisualTestContext::from_window(window.into(), cx);
    vcx.simulate_input(" more");
    vcx.update(|_, cx| request_quit(cx));
    vcx.run_until_parked();
    assert_eq!(
        Store::open(&path)
            .unwrap()
            .workspace()
            .preferences()
            .quick()
            .text,
        "first line\nsecond more",
        "Quit keeps the newest typing"
    );
}

#[gpui::test]
fn enter_creates_a_session_in_the_chosen_project_sends_and_closes(cx: &mut TestAppContext) {
    let _env = crate::tests::codex_env();
    let dir = tempfile::tempdir().unwrap();
    let (path, state) = launch(
        dir.path(),
        |store| {
            store
                .set_quick_draft(QuickDraft {
                    provider: Some(Provider::Codex),
                    model: Some("fake-fast".into()),
                    ..QuickDraft::default()
                })
                .unwrap();
        },
        cx,
    );
    let project = cx.read(|cx| state.read(cx).store().workspace().projects()[0].id());
    let vcx = &mut open_quick(cx);
    vcx.simulate_keystrokes("enter");
    assert!(quick_window(cx).is_some(), "an empty field sends nothing");
    let vcx = &mut VisualTestContext::from_window(quick_window(cx).unwrap().into(), cx);
    vcx.simulate_input("Quick question");
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();
    assert!(quick_window(cx).is_none(), "Enter closed it");

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let reply = loop {
        cx.run_until_parked();
        let reply = cx.read(|cx| {
            let conversation = state.read(cx).conversations().get(&1)?;
            conversation.rows.iter().find_map(|row| match row {
                Row::Item(item)
                    if item.kind == ItemKind::AgentMessage
                        && item.status == ItemStatus::Completed =>
                {
                    Some(item.text.clone())
                }
                _ => None,
            })
        });
        if let Some(reply) = reply {
            break reply;
        }
        assert!(std::time::Instant::now() < deadline, "no reply arrived");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(reply.contains("Quick question"), "{reply}");
    assert!(reply.contains("fake-fast"), "the chosen model ran: {reply}");

    let store = Store::open(&path).unwrap();
    let workspace = store.workspace();
    let session = &workspace.sessions()[0];
    assert_eq!(session.provider(), Provider::Codex);
    assert!(session.started());
    assert!(project.contains(match session.context() {
        keystone_core::SessionContext::Recorded { directory, .. } => directory.path(),
        _ => panic!("the session records its folder"),
    }));
    assert_eq!(
        workspace.selected_in(WindowId::FIRST),
        None,
        "the main window keeps what it showed"
    );
    let view = first_view(cx);
    let listed = cx.read(|cx| view.read(cx).selected(cx));
    assert_eq!(listed, None);
    let quick = workspace.preferences().quick();
    assert_eq!(quick.text, "");
    assert_eq!(quick.provider, Some(Provider::Codex), "the choices stay");
    assert_eq!(quick.project.as_deref(), project.path().to_str());
}

/// A second project beside the one Keystone launches in.
fn with_project(folder: &Path) -> impl FnOnce(&mut Store) {
    let project = keystone_core::ProjectId::new(folder).unwrap();
    move |store| {
        store
            .open_project(WindowId::FIRST, &project, None, None, 1)
            .unwrap();
    }
}

#[gpui::test]
fn each_project_gets_its_own_catalog_when_one_is_chosen_while_another_loads(
    cx: &mut TestAppContext,
) {
    let _env = crate::tests::codex_env();
    let dir = tempfile::tempdir().unwrap();
    let (one, two) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (_, state) = launch(
        dir.path(),
        |store| {
            with_project(one.path())(store);
            with_project(two.path())(store);
            store
                .set_quick_draft(QuickDraft {
                    provider: Some(Provider::Codex),
                    ..QuickDraft::default()
                })
                .unwrap();
        },
        cx,
    );
    let projects: Vec<keystone_core::ProjectId> = cx.read(|cx| {
        let workspace = state.read(cx).store().workspace();
        workspace.projects().iter().map(|p| p.id()).collect()
    });
    assert_eq!(projects.len(), 2, "{projects:?}");
    cx.update(open);
    let window = quick_window(cx).unwrap();
    let keys = window
        .update(cx, |view, _, cx| {
            let first = view.catalog_key().unwrap();
            let next = projects
                .iter()
                .find(|p| Some(*p) != view.project.as_ref())
                .unwrap();
            view.choose_project(next.clone(), cx);
            [first, view.catalog_key().unwrap()]
        })
        .unwrap();
    assert_ne!(keys[0].directory, keys[1].directory);
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        cx.run_until_parked();
        let ready = cx.read(|cx| {
            keys.iter()
                .map(|key| matches!(state.read(cx).catalog_choices(key), Choices::Ready(_)))
                .collect::<Vec<_>>()
        });
        if ready == [true, true] {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "both catalogs answer: {ready:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[gpui::test]
fn a_draft_that_cannot_be_saved_keeps_the_window_open(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (_, state) = launch(dir.path(), |_| {}, cx);
    let vcx = &mut open_quick(cx);
    vcx.simulate_input("unsaved");
    state.update(vcx, |state, _| state.phase = Phase::Exiting);
    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();
    let window = quick_window(cx).expect("Escape kept it open");
    assert_eq!(text(window, cx), "unsaved");
    let error = window.read_with(cx, |view, _| view.error.clone()).unwrap();
    assert!(error.unwrap().starts_with("Draft not saved."));
    state.update(cx, |state, _| state.phase = Phase::Running);
}

#[gpui::test]
fn a_project_whose_folder_is_gone_keeps_the_text_and_creates_nothing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let folder = other.path().join("gone");
    std::fs::create_dir(&folder).unwrap();
    let (_, state) = launch(dir.path(), with_project(&folder), cx);
    std::fs::remove_dir(&folder).unwrap();
    let gone = cx.read(|cx| {
        state
            .read(cx)
            .store()
            .workspace()
            .projects()
            .iter()
            .map(|p| p.id())
            .find(|p| {
                p.path() == folder.canonicalize().unwrap_or(folder.clone()) || p.name() == "gone"
            })
            .unwrap()
    });
    cx.update(open);
    let window = quick_window(cx).unwrap();
    window
        .update(cx, |view, _, cx| view.choose_project(gone, cx))
        .unwrap();
    let vcx = &mut VisualTestContext::from_window(window.into(), cx);
    vcx.simulate_input("keep me");
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();
    assert!(quick_window(cx).is_some(), "it stays open");
    assert_eq!(text(window, cx), "keep me");
    let error = window.read_with(cx, |view, _| view.error.clone()).unwrap();
    assert!(error.unwrap().starts_with("Not sent."));
    let sessions = cx.read(|cx| state.read(cx).store().workspace().sessions().len());
    assert_eq!(sessions, 0);
}

#[gpui::test]
fn a_refused_send_leaves_the_text_with_its_session_shown_in_the_window(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (work, personal) = (dir.path().join("work"), dir.path().join("personal"));
    let (_, state) = launch(dir.path(), |_| {}, cx);
    let ids: Vec<AccountId> = state.update(cx, |state, cx| {
        [("Work", &work), ("Personal", &personal)]
            .map(|(label, home)| {
                state
                    .add_account(Provider::Claude, label, home.to_str().unwrap(), cx)
                    .unwrap()
                    .value
            })
            .into()
    });
    std::fs::remove_dir(&work).unwrap();
    std::os::unix::fs::symlink(&personal, &work).unwrap();
    cx.update(open);
    let window = quick_window(cx).unwrap();
    window
        .update(cx, |view, _, cx| {
            view.choose_account(Provider::Claude, Some(ids[0].clone()), cx)
        })
        .unwrap();
    let vcx = &mut VisualTestContext::from_window(window.into(), cx);
    vcx.simulate_input("keep me");
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();
    assert!(quick_window(cx).is_none(), "it closed");

    let view = first_view(cx);
    let (sessions, draft, error) = cx.read(|cx| {
        let state = state.read(cx);
        let session = state.store().workspace().sessions()[0].clone();
        (
            state.store().workspace().sessions().len(),
            session.draft().to_owned(),
            state.conversations()[&session.id()].error.clone(),
        )
    });
    assert_eq!(sessions, 1);
    assert_eq!(draft, "keep me", "the session holds the text");
    assert!(error.unwrap().contains("overlaps the account"));
    let shown = cx.read(|cx| view.read(cx).selected(cx));
    assert_eq!(shown, Some(1), "the window shows the session");
    let quick = cx.read(|cx| {
        state
            .read(cx)
            .store()
            .workspace()
            .preferences()
            .quick()
            .clone()
    });
    assert_eq!(quick.text, "", "the text is not kept twice");
}
