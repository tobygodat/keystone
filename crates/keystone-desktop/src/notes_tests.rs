use super::*;
use crate::windows::open;
use crate::{AppState, install_app_state, open_windows, request_quit};
use gpui::{AnyWindowHandle, TestAppContext, VisualTestContext};
use keystone_core::{Provider, Store, WindowId, WorkingDirectory};
use std::path::{Path, PathBuf};
use std::time::Duration;

struct Folders {
    _dir: tempfile::TempDir,
    alpha: ProjectId,
    beta: ProjectId,
}

/// Session 1 in alpha and 2 in beta. The first window shows alpha and `selected`.
fn seeded(selected: Option<u64>) -> (Folders, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let make = |name: &str| {
        std::fs::create_dir(root.join(name)).unwrap();
        ProjectId::new(&root.join(name)).unwrap()
    };
    let (alpha, beta) = (make("alpha"), make("beta"));
    let path = root.join("workspace.json");
    let mut store = Store::open(&path).unwrap();
    for (n, project) in [&alpha, &beta].into_iter().enumerate() {
        let directory = WorkingDirectory::new(project.path()).unwrap();
        store
            .create(Provider::Codex, &directory, n as u64 + 1)
            .unwrap();
    }
    store
        .open_project(WindowId::FIRST, &alpha, None, selected, 10)
        .unwrap();
    (
        Folders {
            _dir: dir,
            alpha,
            beta,
        },
        path,
    )
}

fn launch(path: &Path, cx: &mut TestAppContext) -> (Entity<Keystone>, AnyWindowHandle) {
    let store = Store::open(path).unwrap();
    cx.executor().allow_parking();
    cx.update(|cx| {
        keystone_ui::init(cx);
        crate::bind_keys(cx);
        let state: Entity<AppState> = install_app_state(store, cx);
        for (id, placement) in state.read(cx).store().workspace().preferences().windows() {
            open(state.clone(), id, placement, true, false, cx).unwrap();
        }
    });
    cx.run_until_parked();
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

fn note_text(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> String {
    view.read_with(cx, |this, cx| {
        let project = this.presentation.project.as_ref().unwrap();
        this.notes.editors[project]
            .input
            .read(cx)
            .value()
            .to_owned()
    })
}

fn saved(view: &Entity<Keystone>, project: &ProjectId, cx: &mut VisualTestContext) -> String {
    view.read_with(cx, |this, cx| {
        this.state
            .read(cx)
            .store()
            .workspace()
            .note(project)
            .to_owned()
    })
}

fn notes_focused(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> bool {
    view.update_in(cx, |this, window, cx| this.notes_contain_focus(window, cx))
}

fn click(cx: &mut VisualTestContext, id: &'static str) {
    let bounds = cx
        .debug_bounds(id)
        .unwrap_or_else(|| panic!("{id} is shown"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
}

fn switch_project(query: &str, cx: &mut VisualTestContext) {
    cx.simulate_keystrokes("secondary-o");
    cx.simulate_input(query);
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
}

#[gpui::test]
fn a_note_autosaves_on_the_draft_timer_and_quit_saves_the_rest(cx: &mut TestAppContext) {
    let (folders, path) = seeded(Some(1));
    let (view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("secondary-shift-j");
    assert!(cx.debug_bounds("notes-editor").is_some(), "Notes opens");
    assert!(
        cx.debug_bounds("conversation").is_some(),
        "beside the conversation"
    );
    assert!(notes_focused(&view, cx), "with its editor focused");
    cx.simulate_input("Plan");
    cx.simulate_keystrokes("enter");
    cx.simulate_input("then ship");
    assert_eq!(
        saved(&view, &folders.alpha, cx),
        "",
        "not saved per keystroke"
    );
    cx.executor().advance_clock(Duration::from_millis(250));
    cx.run_until_parked();
    assert_eq!(saved(&view, &folders.alpha, cx), "Plan\nthen ship");

    cx.simulate_input(" today");
    cx.update(|_, cx| request_quit(cx));
    cx.run_until_parked();
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.workspace().note(&folders.alpha),
        "Plan\nthen ship today",
        "Quit saves typing the timer had not saved yet"
    );
    assert!(
        store.workspace().note(&folders.beta).is_empty(),
        "the note belongs to the window's project"
    );
}

#[gpui::test]
fn a_relaunch_restores_the_notes_view_and_its_text(app: &mut TestAppContext) {
    let (folders, path) = seeded(Some(1));
    let (view, handle) = launch(&path, app);
    {
        let cx = visual(handle, app);
        cx.simulate_keystrokes("secondary-shift-j");
        cx.simulate_input("Before quitting");
        cx.update(|_, cx| request_quit(cx));
        cx.run_until_parked();
    }
    drop(view);
    assert_eq!(
        Store::open(&path).unwrap().workspace().note(&folders.alpha),
        "Before quitting"
    );

    let (view, handle) = launch(&path, app);
    let cx = visual(handle, app);
    assert!(
        cx.debug_bounds("notes-editor").is_some(),
        "the saved layout brings Notes back"
    );
    assert_eq!(note_text(&view, cx), "Before quitting");
    view.update_in(cx, |this, window, cx| this.focus_notes(window, cx));
    cx.simulate_keystrokes("secondary-end");
    cx.simulate_input(" again");
    assert_eq!(
        note_text(&view, cx),
        "Before quitting again",
        "the editor is usable"
    );
}

#[gpui::test]
fn switching_project_shows_that_projects_note(cx: &mut TestAppContext) {
    let (folders, path) = seeded(Some(1));
    let (view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("secondary-shift-j");
    cx.simulate_input("Alpha note");

    switch_project("beta", cx);
    assert_eq!(
        view.read_with(cx, |this, _| this.presentation.project.clone().unwrap()),
        folders.beta
    );
    cx.simulate_keystrokes("secondary-shift-j");
    assert_eq!(note_text(&view, cx), "", "beta has its own, empty note");
    cx.simulate_input("Beta note");
    cx.executor().advance_clock(Duration::from_millis(250));
    cx.run_until_parked();
    assert_eq!(
        saved(&view, &folders.alpha, cx),
        "Alpha note",
        "alpha's typing saved to alpha after the window left it"
    );
    assert_eq!(saved(&view, &folders.beta, cx), "Beta note");

    switch_project("alpha", cx);
    assert_eq!(note_text(&view, cx), "Alpha note");
}

#[gpui::test]
fn send_to_agent_puts_the_selection_or_the_whole_note_in_the_composer(cx: &mut TestAppContext) {
    let (_folders, path) = seeded(Some(1));
    let (view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("secondary-shift-j");
    cx.simulate_input("Fix the build");
    cx.simulate_keystrokes("shift-left shift-left shift-left shift-left shift-left");
    cx.simulate_keystrokes("secondary-enter");
    let composer = |view: &Entity<Keystone>, cx: &mut VisualTestContext| {
        view.read_with(cx, |this, cx| this.composer.read(cx).value().to_owned())
    };
    assert_eq!(composer(&view, cx), "build", "the selection goes");
    let (draft, composer_focused) = view.update_in(cx, |this, window, cx| {
        (
            this.selected_session(cx).unwrap().draft().to_owned(),
            this.composer.focus_handle(cx).is_focused(window),
        )
    });
    assert_eq!(draft, "build", "saved as the draft, and not sent");
    assert!(composer_focused);
    assert!(
        view.read_with(cx, |this, cx| !this.state.read(cx).turn_running()),
        "nothing is sent"
    );

    view.update_in(cx, |this, window, cx| this.focus_notes(window, cx));
    cx.simulate_keystrokes("end");
    click(cx, "notes-send");
    assert_eq!(
        composer(&view, cx),
        "build\nFix the build",
        "with no selection the whole note goes in at the composer's cursor, on its own line"
    );
}

#[gpui::test]
fn send_to_agent_is_disabled_without_a_session(cx: &mut TestAppContext) {
    let (_folders, path) = seeded(None);
    let (view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("secondary-shift-j");
    cx.simulate_input("Keep this");
    cx.simulate_keystrokes("secondary-enter");
    click(cx, "notes-send");
    assert_eq!(
        view.read_with(cx, |this, cx| this.composer.read(cx).value().to_owned()),
        ""
    );
    assert_eq!(note_text(&view, cx), "Keep this");
}

#[gpui::test]
fn the_surface_menu_opens_notes_as_a_tab_and_toggle_closes_it(cx: &mut TestAppContext) {
    let (folders, path) = seeded(Some(1));
    let (view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.simulate_keystrokes("secondary-shift-e");
    click(cx, "surface-add");
    click(cx, "surface-add-notes");
    assert!(cx.debug_bounds("notes-editor").is_some(), "Notes opens");
    let together = view.read_with(cx, |this, cx| {
        let c = this.shown_composition(cx);
        let view_of = |component| c.views().find(|(_, k)| *k == component).unwrap().0;
        crate::arrangement::tabs_holding(c.root(), view_of(Component::Files))
            .is_some_and(|(views, _)| views.contains(&view_of(Component::Notes)))
    });
    assert!(together, "as a tab in Files' strip");
    assert!(notes_focused(&view, cx));
    click(cx, "surface-add");
    assert!(
        cx.debug_bounds("surface-add-notes").is_none(),
        "a layout holds one Notes view"
    );
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();

    view.update_in(cx, |this, window, cx| this.focus_notes(window, cx));
    cx.simulate_input("Tabbed");
    cx.simulate_keystrokes("secondary-shift-j");
    assert!(
        cx.debug_bounds("notes-editor").is_none(),
        "with the editor focused, the shortcut closes Notes"
    );
    cx.executor().advance_clock(Duration::from_millis(250));
    cx.run_until_parked();
    assert_eq!(saved(&view, &folders.alpha, cx), "Tabbed");
    cx.simulate_keystrokes("secondary-shift-j");
    assert_eq!(note_text(&view, cx), "Tabbed", "reopening keeps the note");
}

#[gpui::test]
fn opening_notes_that_makes_the_window_fit_its_views_as_tabs_shows_it_in_front(
    cx: &mut TestAppContext,
) {
    let (_folders, path) = seeded(None);
    let (view, handle) = launch(&path, cx);
    let cx = visual(handle, cx);
    cx.simulate_resize(gpui::size(px(720.), px(800.)));
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("fitted-tabs").is_none(),
        "the layout fits before Notes opens"
    );
    cx.simulate_keystrokes("secondary-shift-j");
    assert!(
        cx.debug_bounds("fitted-tabs").is_some(),
        "Notes makes it fit as tabs"
    );
    assert!(
        cx.debug_bounds("notes-editor").is_some(),
        "the Notes tab is in front"
    );
    assert!(notes_focused(&view, cx));
}

#[gpui::test]
fn two_windows_on_one_project_share_its_note_and_closing_one_saves_it(cx: &mut TestAppContext) {
    let (folders, path) = seeded(Some(1));
    let (first, first_handle) = launch(&path, cx);
    first_handle
        .update(cx, |_, window, cx| {
            first.update(cx, |this, cx| this.open_new_window(Some(1), window, cx))
        })
        .unwrap();
    cx.run_until_parked();
    let second = cx
        .update(|cx| open_windows(cx))
        .into_iter()
        .find(|view| view != &first)
        .unwrap();
    let second_handle = cx.read(|cx| second.read(cx).handle);
    {
        let cx = visual(second_handle, cx);
        cx.simulate_keystrokes("secondary-shift-j");
        assert_eq!(note_text(&second, cx), "");
    }
    {
        let cx = visual(first_handle, cx);
        cx.simulate_keystrokes("secondary-shift-j");
        cx.simulate_input("Shared");
        assert_eq!(saved(&first, &folders.alpha, cx), "", "not saved yet");
    }
    let mirrored = cx.read(|cx| {
        let second = second.read(cx);
        second.notes.editors[&folders.alpha]
            .input
            .read(cx)
            .value()
            .to_owned()
    });
    assert_eq!(mirrored, "Shared", "the other window shows the typing");

    let root = first.downgrade();
    let closed = first_handle
        .update(cx, |_, window, cx| {
            let closed = crate::close_window(&root, window, cx);
            if closed {
                window.remove_window();
            }
            closed
        })
        .unwrap();
    cx.run_until_parked();
    assert!(closed);
    assert_eq!(
        Store::open(&path).unwrap().workspace().note(&folders.alpha),
        "Shared",
        "closing the window saved typing its timer had not"
    );
    cx.executor().advance_clock(Duration::from_millis(250));
    cx.run_until_parked();
    assert_eq!(
        Store::open(&path).unwrap().workspace().note(&folders.alpha),
        "Shared"
    );
}
