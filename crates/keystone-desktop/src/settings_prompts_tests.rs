//! The prompt library in Settings → Prompts and the palette's Insert prompt…,
//! through the window.

use crate::{Keystone, bind_keys};
use gpui::{AppContext, Entity, Focusable, Modifiers, TestAppContext, VisualTestContext};
use keystone_core::{PromptFields, Provider, Store, WorkingDirectory};
use std::path::Path;

/// One selected Claude session working in `dir/project`, with `draft`.
fn seeded(dir: &Path, draft: &str) -> std::path::PathBuf {
    let folder = dir.join("project");
    std::fs::create_dir_all(&folder).unwrap();
    let path = dir.join("workspace.json");
    let mut store = Store::open(&path).unwrap();
    let id = store
        .create(
            Provider::Claude,
            &WorkingDirectory::new(&folder).unwrap(),
            1,
        )
        .unwrap()
        .value;
    store.set_draft(id, draft.into()).unwrap();
    store.select(id).unwrap();
    path
}

fn open_window<'a>(
    path: &Path,
    cx: &'a mut TestAppContext,
) -> (Entity<Keystone>, &'a mut VisualTestContext) {
    let store = Store::open(path).unwrap();
    cx.update(|cx| {
        keystone_ui::init(cx);
        bind_keys(cx);
    });
    cx.executor().allow_parking();
    let (view, cx) = cx.add_window_view(|window, cx| {
        Keystone::new(
            cx.new(|cx| {
                crate::AppState::new(store, cx).with_test_programs(crate::test_support::scripted())
            }),
            false,
            window,
            cx,
        )
    });
    view.update_in(cx, |this, window, cx| {
        this.composer.focus_handle(cx).focus(window, cx)
    });
    cx.run_until_parked();
    (view, cx)
}

fn shown(cx: &mut VisualTestContext, selector: &str) -> bool {
    cx.debug_bounds(selector.to_owned().leak()).is_some()
}

fn click(cx: &mut VisualTestContext, selector: &str) {
    let bounds = cx
        .debug_bounds(selector.to_owned().leak())
        .unwrap_or_else(|| panic!("{selector} rendered"));
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
}

fn keys(cx: &mut VisualTestContext, keys: &str) {
    cx.simulate_keystrokes(keys);
    cx.run_until_parked();
}

/// Each saved prompt's title, shortcut, project folder name and use count, in order.
fn saved(path: &Path) -> Vec<(String, Option<String>, Option<String>)> {
    Store::open(path)
        .unwrap()
        .workspace()
        .prompts()
        .iter()
        .map(|p| {
            (
                p.title().to_owned(),
                p.shortcut().map(str::to_owned),
                p.project().map(|project| {
                    project
                        .path()
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned()
                }),
            )
        })
        .collect()
}

fn draft(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> String {
    view.read_with(cx, |view, cx| view.composer.read(cx).value().to_owned())
}

/// Add from the form, edit, reorder, refuse a clashing shortcut, and delete; every
/// change is saved as it happens.
#[gpui::test]
fn the_library_adds_edits_reorders_and_deletes_prompts(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), "");
    let (view, cx) = open_window(&path, cx);
    keys(cx, "secondary-,");
    click(cx, "settings-section-prompts");
    assert!(shown(cx, "settings-prompts"));

    click(cx, "prompt-title");
    cx.simulate_input("Review");
    click(cx, "prompt-shortcut");
    cx.simulate_input("/rev");
    click(cx, "prompt-body");
    cx.simulate_input("Review this diff.");
    keys(cx, "shift-enter");
    cx.simulate_input("Be terse.");
    click(cx, "prompt-save");
    assert_eq!(saved(&path), [("Review".into(), Some("rev".into()), None)]);
    let review = Store::open(&path).unwrap().workspace().prompts()[0].id();
    assert_eq!(
        Store::open(&path).unwrap().workspace().prompts()[0].body(),
        "Review this diff.\nBe terse."
    );
    assert_eq!(
        view.read_with(cx, |view, cx| view
            .settings
            .as_ref()
            .unwrap()
            .prompts
            .title
            .read(cx)
            .value()
            .to_owned()),
        "",
        "the form empties for the next prompt"
    );

    // A second prompt, for this window's project only.
    click(cx, "prompt-title");
    cx.simulate_input("Plan");
    click(cx, "prompt-body");
    cx.simulate_input("Plan first.");
    click(cx, "prompt-scope");
    click(cx, "prompt-scope-option-1");
    keys(cx, "tab");
    click(cx, "prompt-save");
    let window_project = view.read_with(cx, |view, _| {
        view.presentation
            .project
            .as_ref()
            .unwrap()
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    });
    assert_eq!(saved(&path)[1], ("Plan".into(), None, Some(window_project)));
    let plan = Store::open(&path).unwrap().workspace().prompts()[1].id();

    click(cx, &format!("prompt-up-{plan}"));
    assert_eq!(saved(&path)[0].0, "Plan");
    click(cx, &format!("prompt-down-{plan}"));
    assert_eq!(saved(&path)[0].0, "Review");

    click(cx, &format!("prompt-edit-{review}"));
    keys(cx, "secondary-a");
    cx.simulate_input("Review closely");
    keys(cx, "enter");
    assert_eq!(
        saved(&path)[0],
        ("Review closely".into(), Some("rev".into()), None)
    );

    click(cx, "prompt-title");
    cx.simulate_input("Clash");
    click(cx, "prompt-shortcut");
    cx.simulate_input("rev");
    click(cx, "prompt-body");
    cx.simulate_input("x");
    click(cx, "prompt-save");
    view.read_with(cx, |view, _| {
        let error = view.error.as_deref().unwrap();
        assert!(
            error.starts_with("Prompt not saved. /rev already inserts"),
            "{error}"
        );
    });
    assert!(shown(cx, "settings-error"));
    assert_eq!(saved(&path).len(), 2);

    click(cx, &format!("prompt-delete-{plan}"));
    assert_eq!(saved(&path).len(), 1);
    assert!(!shown(cx, &format!("prompt-row-{plan}")));
}

/// A shortcut that names a Terminal only built-in, or a command the session's account
/// lists, says the command wins.
#[gpui::test]
fn the_library_shows_when_a_command_wins_over_a_shortcut(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), "");
    let id = Store::open(&path)
        .unwrap()
        .add_prompt(PromptFields {
            title: "Sign in notes".into(),
            body: "notes".into(),
            shortcut: "Login".into(),
            project: None,
        })
        .unwrap()
        .value;
    let (_view, cx) = open_window(&path, cx);
    keys(cx, "secondary-,");
    click(cx, "settings-section-prompts");
    assert!(shown(cx, &format!("prompt-conflict-{id}")));
}

fn add(
    view: &Entity<Keystone>,
    cx: &mut VisualTestContext,
    title: &str,
    body: &str,
    project: Option<&Path>,
) -> u64 {
    view.update(cx, |view, cx| {
        view.state.update(cx, |state, cx| {
            state
                .change_prompts(
                    |store| {
                        store.add_prompt(PromptFields {
                            title: title.into(),
                            body: body.into(),
                            shortcut: String::new(),
                            project: project.map(|p| keystone_core::ProjectId::new(p).unwrap()),
                        })
                    },
                    cx,
                )
                .unwrap()
                .value
        })
    })
}

/// A Save from a form whose prompt another window changed or deleted since Edit
/// loaded it overwrites nothing.
#[gpui::test]
fn saving_an_edit_never_overwrites_a_change_from_another_window(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), "");
    let (view, cx) = open_window(&path, cx);
    let id = add(&view, cx, "Original", "Body.", None);
    keys(cx, "secondary-,");
    click(cx, "settings-section-prompts");
    click(cx, &format!("prompt-edit-{id}"));
    let elsewhere = |view: &Entity<Keystone>, cx: &mut VisualTestContext, title: &str| {
        view.update(cx, |view, cx| {
            view.state.update(cx, |state, cx| {
                state
                    .change_prompts(
                        |store| {
                            store.edit_prompt(
                                id,
                                PromptFields {
                                    title: title.into(),
                                    body: "Body.".into(),
                                    shortcut: String::new(),
                                    project: None,
                                },
                            )
                        },
                        cx,
                    )
                    .unwrap()
            })
        });
    };
    elsewhere(&view, cx, "From the other window");
    cx.run_until_parked();
    click(cx, "prompt-title");
    cx.simulate_input(" edited");
    click(cx, "prompt-save");
    assert_eq!(saved(&path)[0].0, "From the other window");
    view.read_with(cx, |view, _| {
        assert!(
            view.error
                .as_deref()
                .unwrap()
                .contains("changed in another window")
        );
    });

    // Edit again loads it; a deletion elsewhere then resets the form on Save.
    click(cx, &format!("prompt-edit-{id}"));
    view.update(cx, |view, cx| {
        view.state.update(cx, |state, cx| {
            state
                .change_prompts(|store| store.remove_prompt(id), cx)
                .unwrap()
        })
    });
    cx.run_until_parked();
    click(cx, "prompt-save");
    view.read_with(cx, |view, cx| {
        assert!(
            view.error
                .as_deref()
                .unwrap()
                .contains("deleted in another window")
        );
        let page = &view.settings.as_ref().unwrap().prompts;
        assert!(page.editing.is_none());
        assert_eq!(page.title.read(cx).value(), "");
    });
    assert!(saved(&path).is_empty());
}

/// Without a selected session there is no composer to fill, so Insert prompt…
/// opens the library instead and inserts nothing.
#[gpui::test]
fn insert_prompt_opens_the_library_on_the_welcome_screen(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    drop(Store::open(&path).unwrap());
    let (view, cx) = open_window(&path, cx);
    add(&view, cx, "Greeting", "Hello there.", None);
    cx.run_until_parked();
    view.update_in(cx, |view, window, cx| view.focus.focus(window, cx));
    keys(cx, "secondary-shift-i");
    assert!(!shown(cx, "prompts"));
    assert!(shown(cx, "settings-prompts"));
    keys(cx, "escape");
    view.update_in(cx, |view, window, cx| {
        view.insert_prompt(1, false, window, cx)
    });
    assert_eq!(draft(&view, cx), "");
}

/// Escape closes the prompt list first, even over an open `/` menu, and a row whose
/// prompt another window moved to another project inserts nothing.
#[gpui::test]
fn the_prompt_list_takes_escape_and_skips_a_prompt_no_longer_offered(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), "");
    let (view, cx) = open_window(&path, cx);
    let id = add(&view, cx, "Greeting", "Hello there.", None);
    cx.run_until_parked();
    cx.simulate_input("/gre");
    cx.run_until_parked();
    assert!(shown(cx, "slash-menu"));
    keys(cx, "secondary-shift-i");
    assert!(shown(cx, "prompts"));
    assert!(!shown(cx, "slash-menu"), "the list hides the menu under it");
    keys(cx, "escape");
    assert!(!shown(cx, "prompts"), "one Escape closes the list");
    assert!(shown(cx, "slash-menu"), "the menu is back");
    assert_eq!(draft(&view, cx), "/gre");

    keys(cx, "secondary-a backspace");
    keys(cx, "secondary-shift-i");
    let other = dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    view.update(cx, |view, cx| {
        view.state.update(cx, |state, cx| {
            state
                .change_prompts(
                    |store| {
                        store.edit_prompt(
                            id,
                            PromptFields {
                                title: "Greeting".into(),
                                body: "Hello there.".into(),
                                shortcut: String::new(),
                                project: Some(keystone_core::ProjectId::new(&other).unwrap()),
                            },
                        )
                    },
                    cx,
                )
                .unwrap()
        })
    });
    cx.run_until_parked();
    keys(cx, "enter");
    assert!(!shown(cx, "prompts"));
    assert_eq!(
        draft(&view, cx),
        "",
        "a prompt no longer offered is not inserted"
    );
    view.read_with(cx, |view, _| {
        let error = view.error.as_deref().unwrap();
        assert!(error.contains("Prompt no longer available"), "{error}");
    });
}

/// Insert prompt… lists the prompts offered here and inserts at the caret without
/// sending; with none saved it opens the library.
#[gpui::test]
fn the_palette_inserts_a_prompt_at_the_caret(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path(), "");
    let (view, cx) = open_window(&path, cx);
    keys(cx, "secondary-k");
    cx.simulate_input("Insert prompt");
    keys(cx, "enter");
    assert!(
        shown(cx, "settings-prompts"),
        "with none saved, the library opens"
    );
    keys(cx, "escape");

    let other = dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    view.update(cx, |view, cx| {
        view.state.update(cx, |state, cx| {
            for (title, body, project) in [
                ("Greeting", "Hello there.", None),
                ("Elsewhere", "Not here.", Some(&other)),
            ] {
                let project = project.map(|p| keystone_core::ProjectId::new(p).unwrap());
                state
                    .change_prompts(
                        |store| {
                            store.add_prompt(PromptFields {
                                title: title.into(),
                                body: body.into(),
                                shortcut: String::new(),
                                project,
                            })
                        },
                        cx,
                    )
                    .unwrap();
            }
        })
    });
    cx.run_until_parked();
    cx.simulate_input("Draft: ");
    keys(cx, "secondary-shift-i");
    assert!(shown(cx, "prompts"));
    let rows = view.read_with(cx, |view, cx| {
        view.prompt_list
            .as_ref()
            .unwrap()
            .state
            .read(cx)
            .matches()
            .map(|item| item.label.to_string())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        rows,
        ["Greeting"],
        "another project's prompt is not offered"
    );
    cx.simulate_input("greet");
    keys(cx, "enter");
    assert!(!shown(cx, "prompts"));
    assert_eq!(draft(&view, cx), "Draft: Hello there.");
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.workspace().sessions()[0].draft(),
        "Draft: Hello there."
    );
    assert!(
        !store.workspace().sessions()[0].started(),
        "inserting never sends"
    );

    // Again, from the palette's own row.
    keys(cx, "secondary-k");
    cx.simulate_input("Insert prompt");
    keys(cx, "enter");
    assert!(shown(cx, "prompts"));
    keys(cx, "enter");
    assert_eq!(draft(&view, cx), "Draft: Hello there.Hello there.");
}
