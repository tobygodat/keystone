//! The `/` menu through the window, with the scripted providers listing fixture
//! commands and skills from an added account's home and the session folder.

use super::*;
use crate::AppState;
use gpui::{TestAppContext, VisualTestContext};
use keystone_core::{Store, WorkingDirectory};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A scripted provider with short delays and, for Claude, its message log in `dir`.
fn wrapper(dir: &Path, name: &str) -> PathBuf {
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(format!("keystone-fake-{name}"));
    let environment = serde_json::json!({
        "KEYSTONE_FAKE_CODEX_DELAY_MS": "2",
        "KEYSTONE_FAKE_CLAUDE_DELAY_MS": "2",
        "KEYSTONE_FAKE_CLAUDE_LOG_DIR": dir.to_str().unwrap(),
    });
    let script = dir.join(format!("{name}-slash-wrapper"));
    std::fs::write(
        &script,
        format!(
            "#!/usr/bin/python3\nimport os, sys\nos.environ.update({environment})\nbinary = {}\nos.execv(binary, [binary, *sys.argv[1:]])\n",
            serde_json::to_string(&binary).unwrap()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// One selected `provider` session on an added account whose home holds a fixture
/// command and skill, in a folder with a project command, its composer focused.
fn open(
    provider: Provider,
    cx: &mut TestAppContext,
) -> (tempfile::TempDir, Entity<Keystone>, &mut VisualTestContext) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let home = root.join("home");
    let folder = root.join("project");
    write(
        &home.join("commands/fixture-review.md"),
        "---\ndescription: Fixture user command\nargument-hint: [file]\n---\nReview\n",
    );
    write(
        &home.join("skills/fixture-skill/SKILL.md"),
        "---\nname: fixture-skill\ndescription: Fixture user skill\n---\nSkill.\n",
    );
    write(
        &folder.join(".claude/commands/fixture-project.md"),
        "---\ndescription: Fixture project command\n---\nProject\n",
    );
    // The scripted Claude logs each user message it reads here.
    write(&folder.join(".fake-claude-messages"), "");
    let mut store = Store::open(root.join("workspace.json")).unwrap();
    let account = store
        .add_account(provider, "Work", home.clone(), &[])
        .unwrap()
        .value;
    let directory = WorkingDirectory::new(&folder).unwrap();
    let id = store.create(provider, &directory, 1).unwrap().value;
    store.set_account(id, Some(account)).unwrap();
    store.select(id).unwrap();
    cx.update(|cx| {
        keystone_ui::init(cx);
        crate::bind_keys(cx);
    });
    cx.executor().allow_parking();
    let programs = (wrapper(root, "codex"), wrapper(root, "claude"));
    let (view, cx) = cx.add_window_view(|window, cx| {
        let state = cx.new(|cx| {
            let mut state = AppState::new(store, cx);
            state.test_programs = Some(programs);
            state
        });
        Keystone::new(state, false, window, cx)
    });
    view.update_in(cx, |this, window, cx| {
        this.composer.focus_handle(cx).focus(window, cx)
    });
    cx.run_until_parked();
    (dir, view, cx)
}

/// Runs the app with real time passing until `done` holds.
fn wait(
    cx: &mut VisualTestContext,
    view: &Entity<Keystone>,
    done: impl Fn(&Keystone, &App) -> bool,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        cx.run_until_parked();
        if view.read_with(cx, |view, cx| done(view, cx)) {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "timed out");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn shown(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> Vec<(String, &'static str)> {
    view.read_with(cx, |view, cx| {
        view.slash_entries(cx)
            .map(|(.., entries)| {
                entries
                    .into_iter()
                    .map(|e| (e.item.label().to_owned(), source_label(&e.item)))
                    .collect()
            })
            .unwrap_or_default()
    })
}

fn draft(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> String {
    view.read_with(cx, |view, cx| view.composer.read(cx).value().to_owned())
}

fn listed(view: &Keystone, cx: &App) -> bool {
    matches!(
        view.state
            .read(cx)
            .commands(1, view.provider(1, cx).unwrap()),
        Some(Commands::Ready(_))
    )
}

/// `/` asks the account before any Send and lists its commands and skills with their
/// sources. Typing filters, Enter inserts the command and a space, the hint shows
/// what it takes, and Send delivers the text as typed.
#[gpui::test]
fn a_claude_command_is_listed_before_the_first_send_inserted_and_sent_as_typed(
    cx: &mut TestAppContext,
) {
    let (dir, view, cx) = open(Provider::Claude, cx);
    cx.simulate_input("/");
    assert!(
        cx.debug_bounds("slash-menu").is_some(),
        "the menu opens at once"
    );
    wait(cx, &view, listed);
    let all = shown(&view, cx);
    assert_eq!(
        &all[..5],
        [
            ("/fixture-skill".into(), "Skill"),
            ("/fixture-review".into(), "User"),
            ("/fixture-project".into(), "Project"),
            ("/compact".into(), ""),
            ("/context".into(), ""),
        ]
    );
    assert!(all.contains(&("/login".into(), "Terminal only")));
    assert!(cx.debug_bounds("slash-menu-list-row-0").is_some());

    cx.simulate_input("fixrev");
    assert_eq!(shown(&view, cx)[0].0, "/fixture-review");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(draft(&view, cx), "/fixture-review ");
    assert!(
        cx.debug_bounds("slash-menu").is_none(),
        "choosing closes it"
    );
    view.read_with(cx, |view, cx| {
        assert_eq!(
            view.slash_hint(1, Provider::Claude, cx).as_deref(),
            Some("/fixture-review file")
        );
        assert_eq!(
            Store::open(dir.path().join("workspace.json"))
                .unwrap()
                .workspace()
                .sessions()[0]
                .draft(),
            "/fixture-review ",
            "the inserted draft is saved"
        );
    });

    cx.simulate_input("src/lib.rs");
    cx.simulate_keystrokes("ctrl-enter");
    let messages = dir.path().join("project/.fake-claude-messages");
    wait(cx, &view, |_, _| {
        std::fs::read_to_string(&messages).is_ok_and(|m| !m.is_empty())
    });
    assert_eq!(
        std::fs::read_to_string(&messages).unwrap().trim(),
        "\"/fixture-review src/lib.rs\""
    );
}

/// Up and Down move the highlight without moving the caret, Tab chooses like Enter,
/// and Escape closes the menu until the draft changes.
#[gpui::test]
fn the_keyboard_drives_the_menu_while_the_composer_keeps_focus(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open(Provider::Claude, cx);
    cx.simulate_input("/fixture");
    wait(cx, &view, listed);
    let highlighted = |view: &Entity<Keystone>, cx: &mut VisualTestContext| {
        view.read_with(cx, |view, cx| view.slash.list.read(cx).highlighted())
    };
    assert_eq!(highlighted(&view, cx), Some(0));
    cx.simulate_keystrokes("down down up");
    assert_eq!(highlighted(&view, cx), Some(1));
    assert_eq!(draft(&view, cx), "/fixture", "arrows leave the draft alone");
    cx.simulate_modifiers_change(gpui::Modifiers::shift());
    cx.simulate_keystrokes("shift-enter");
    cx.simulate_modifiers_change(gpui::Modifiers::none());
    assert_eq!(
        draft(&view, cx),
        "/fixture\n",
        "Shift+Enter keeps its newline"
    );
    cx.simulate_keystrokes("backspace");
    cx.run_until_parked();

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(cx.debug_bounds("slash-menu").is_none(), "Escape closes it");
    assert_eq!(draft(&view, cx), "/fixture");
    cx.simulate_keystrokes("enter");
    assert_eq!(
        draft(&view, cx),
        "/fixture\n",
        "a closed menu leaves Enter alone"
    );
    cx.simulate_keystrokes("backspace");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("slash-menu").is_some(),
        "an edit opens it again"
    );
    assert_eq!(highlighted(&view, cx), Some(0));
    cx.simulate_keystrokes("tab");
    cx.run_until_parked();
    assert_eq!(draft(&view, cx), format!("{} ", shown_first(&view, cx)));
}

fn shown_first(view: &Entity<Keystone>, cx: &mut VisualTestContext) -> String {
    view.read_with(cx, |view, cx| {
        let first = entries(
            view.state.read(cx).commands(1, Provider::Claude),
            Provider::Claude,
            "fixture",
            &[],
        );
        first[0].item.label().to_owned()
    })
}

/// A Terminal only command is neither inserted nor sent: choosing or sending it shows
/// how to open the account's own terminal in the session folder, and Copy copies it.
#[gpui::test]
fn a_terminal_only_command_offers_the_terminal_and_is_never_sent(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(Provider::Claude, cx);
    cx.simulate_input("/logi");
    wait(cx, &view, listed);
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(draft(&view, cx), "/logi", "nothing is inserted");
    assert!(cx.debug_bounds("slash-terminal").is_some());
    let expected = format!(
        "cd {} && env -u ANTHROPIC_API_KEY -u ANTHROPIC_AUTH_TOKEN -u CLAUDE_CODE_OAUTH_TOKEN \
         -u CLAUDE_CODE_USE_BEDROCK -u CLAUDE_CODE_USE_VERTEX -u CLAUDE_CODE_USE_FOUNDRY \
         CLAUDE_CONFIG_DIR={} claude",
        dir.path().join("project").display(),
        dir.path().join("home").display()
    );
    let copy = cx.debug_bounds("slash-terminal-copy").unwrap();
    cx.simulate_click(copy.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert_eq!(
        cx.read_from_clipboard().and_then(|c| c.text()),
        Some(expected.clone())
    );

    cx.simulate_input("n now");
    assert!(
        cx.debug_bounds("slash-terminal").is_none(),
        "an edit hides it"
    );
    cx.simulate_keystrokes("ctrl-enter");
    cx.run_until_parked();
    assert_eq!(draft(&view, cx), "/login now", "the draft stays");
    assert!(cx.debug_bounds("slash-terminal").is_some());
    view.read_with(cx, |view, cx| {
        let conversation = view.state.read(cx).conversations().get(&1);
        assert!(
            conversation.is_none_or(|c| !c.running && c.rows.is_empty()),
            "nothing sent"
        );
        assert!(!view.state.read(cx).session(1).unwrap().started());
    });
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(cx.debug_bounds("slash-terminal").is_none());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("project/.fake-claude-messages")).unwrap(),
        ""
    );
}

/// A Codex session lists the account's and the folder's skills from `skills/list`
/// and inserts one as Codex invokes it, `$name`.
#[gpui::test]
fn a_codex_session_lists_its_skills_and_inserts_one_as_a_dollar_mention(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(Provider::Codex, cx);
    write(
        &dir.path().join("home/skills/fixture-codex-skill/SKILL.md"),
        "---\nname: fixture-codex-skill\ndescription: Fixture Codex skill\n---\nSkill.\n",
    );
    cx.simulate_input("/codex");
    wait(cx, &view, listed);
    assert_eq!(
        shown(&view, cx)[0],
        ("$fixture-codex-skill".into(), "Skill")
    );
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(draft(&view, cx), "$fixture-codex-skill ");

    view.update_in(cx, |view, _, cx| {
        view.composer
            .update(cx, |input, cx| input.set_value(String::new(), cx))
    });
    cx.simulate_input("/review");
    cx.run_until_parked();
    assert!(shown(&view, cx).contains(&("/review".into(), "Terminal only")));
}

/// A Terminal only notice names one account's terminal, so moving the unsent session
/// to another account or provider hides it instead of offering the old command.
#[gpui::test]
fn a_terminal_only_notice_closes_when_the_session_changes_account(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open(Provider::Claude, cx);
    cx.simulate_input("/logi");
    wait(cx, &view, listed);
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(cx.debug_bounds("slash-terminal").is_some());
    view.update(cx, |view, cx| {
        view.state.update(cx, |state, cx| {
            state
                .set_provider_account(1, Provider::Claude, None, cx)
                .unwrap();
        })
    });
    cx.run_until_parked();
    assert_eq!(draft(&view, cx), "/logi");
    assert!(
        cx.debug_bounds("slash-terminal").is_none(),
        "the old account's command is not offered"
    );
}

/// A list that failed while the models loaded has its own Retry, which asks again.
#[gpui::test]
fn a_failed_command_list_can_be_retried_from_the_menu(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open(Provider::Codex, cx);
    cx.simulate_input("/");
    wait(cx, &view, listed);
    view.update(cx, |view, cx| {
        view.state.update(cx, |state, _| {
            let key = state.catalog_key(1, Provider::Codex).unwrap();
            let account = key.account.clone();
            let catalogs = state.test_catalogs_mut();
            catalogs.clear(&key);
            assert!(catalogs.ask(key, 1));
            let failed = Commands::Failed("skills/list failed".into());
            catalogs.apply_commands(Provider::Codex, account.clone(), 1, failed);
            let models = keystone_agent::Catalog::Ready {
                models: Vec::new(),
                default_model: None,
            };
            catalogs.apply(Provider::Codex, account, 1, models);
        });
        cx.notify();
    });
    cx.run_until_parked();
    assert!(!view.read_with(cx, |view, cx| listed(view, cx)));
    let retry = cx.debug_bounds("slash-menu-retry").expect("a Retry button");
    cx.simulate_click(retry.center(), gpui::Modifiers::none());
    wait(cx, &view, listed);
    assert!(cx.debug_bounds("slash-menu-retry").is_none());
}

/// A session whose folder this version cannot use says why instead of loading forever.
#[gpui::test]
fn a_session_without_a_usable_folder_says_why_it_lists_nothing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let directory = WorkingDirectory::new(dir.path()).unwrap();
    let mut store = Store::in_memory().unwrap();
    store.create(Provider::Claude, &directory, 1).unwrap();
    store.create(Provider::Claude, &directory, 2).unwrap();
    store.select(1).unwrap();
    let mut value = store.saved_snapshot().unwrap();
    value["workspace"]["sessions"][0]
        .as_object_mut()
        .unwrap()
        .remove("execution_context");
    value["workspace"]["sessions"][1]["execution_context"] = serde_json::json!({"version": 700});
    let path = dir.path().join("workspace.json");
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let store = Store::open(&path).unwrap();
    cx.update(|cx| {
        keystone_ui::init(cx);
        crate::bind_keys(cx);
    });
    cx.executor().allow_parking();
    let (view, cx) = cx.add_window_view(|window, cx| {
        Keystone::new(cx.new(|cx| AppState::new(store, cx)), false, window, cx)
    });
    let status = |view: &Entity<Keystone>, id: u64, cx: &mut VisualTestContext| {
        view.update_in(cx, |view, window, cx| {
            view.select(id, cx);
            view.composer
                .update(cx, |input, cx| input.set_value("/".into(), cx));
            view.composer.focus_handle(cx).focus(window, cx);
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("slash-menu").is_some());
        view.read_with(cx, |view, cx| view.slash_status(id, Provider::Claude, cx))
    };
    assert_eq!(
        status(&view, 1, cx).as_deref(),
        Some("Choose a conversation folder to list commands.")
    );
    assert_eq!(
        status(&view, 2, cx).as_deref(),
        Some("Commands unavailable for this folder.")
    );
}

/// Sent before anything listed the account's commands, a Terminal only name waits for
/// the list, and the account's own command of that name then sends.
#[gpui::test]
fn an_account_command_named_like_a_terminal_only_one_sends_once_listed(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(Provider::Claude, cx);
    write(
        &dir.path().join("home/commands/status.md"),
        "---\ndescription: Fixture status command\n---\nStatus\n",
    );
    view.update_in(cx, |view, _, cx| {
        view.composer
            .update(cx, |input, cx| input.set_value("/status now".into(), cx))
    });
    cx.run_until_parked();
    cx.simulate_keystrokes("ctrl-enter");
    wait(cx, &view, listed);
    assert_eq!(draft(&view, cx), "/status now", "the first Send waits");
    cx.simulate_keystrokes("ctrl-enter");
    wait(cx, &view, |view, cx| {
        view.state.read(cx).session(1).unwrap().started()
    });
}

/// Saved prompts this window's project offers join the menu with source Saved, by
/// shortcut or title; another project's are left out. A command of the same name is
/// listed first and its prompt says the command wins. Choosing a prompt fills the
/// composer with its text, counts the use and sends nothing.
#[gpui::test]
fn saved_prompts_join_the_menu_and_a_command_of_the_same_name_wins(cx: &mut TestAppContext) {
    let (dir, view, cx) = open(Provider::Claude, cx);
    let project = keystone_core::ProjectId::new(&dir.path().join("project")).unwrap();
    let other = dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let other = keystone_core::ProjectId::new(&other).unwrap();
    let body = "Check the tests.\nCheck the docs.";
    view.update(cx, |view, cx| {
        view.state.update(cx, |state, cx| {
            for (title, text, shortcut, project) in [
                ("Review checklist", body, "rev", None),
                (
                    "Project notes",
                    "Mind the layout.",
                    "",
                    Some(project.clone()),
                ),
                ("Elsewhere", "Not here.", "elsewhere", Some(other.clone())),
                ("Shadowed", "Mine.", "/fixture-review", None),
            ] {
                state
                    .change_prompts(
                        |store| {
                            store.add_prompt(keystone_core::PromptFields {
                                title: title.into(),
                                body: text.into(),
                                shortcut: shortcut.into(),
                                project,
                            })
                        },
                        cx,
                    )
                    .unwrap();
            }
        })
    });
    cx.simulate_input("/");
    wait(cx, &view, listed);
    let all = shown(&view, cx);
    assert_eq!(
        &all[..2],
        [("/rev".into(), "Saved"), ("Project notes".into(), "Saved")]
    );
    let at = |row: (&str, &str)| all.iter().position(|(l, s)| (l.as_str(), *s) == row);
    assert!(
        at(("/fixture-review", "User")) < at(("/fixture-review", "Saved · command wins")),
        "the command comes first without a query too: {all:?}"
    );
    assert_eq!(all.last().unwrap().1, "Saved · command wins");
    assert!(!all.iter().any(|(label, _)| label == "/elsewhere"));

    cx.simulate_input("fixture-review");
    assert_eq!(
        &shown(&view, cx)[..2],
        [
            ("/fixture-review".into(), "User"),
            ("/fixture-review".into(), "Saved · command wins"),
        ],
        "the command comes first"
    );

    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("/rev");
    assert_eq!(shown(&view, cx)[0], ("/rev".into(), "Saved"));
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        draft(&view, cx),
        body,
        "the composer holds the prompt's text"
    );
    let saved = Store::open(dir.path().join("workspace.json")).unwrap();
    assert_eq!(saved.workspace().sessions()[0].draft(), body);
    assert!(!saved.workspace().sessions()[0].started());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("project/.fake-claude-messages")).unwrap(),
        "",
        "nothing is sent"
    );

    // The library says so too, once the account has listed its commands.
    let shadowed = saved.workspace().prompts()[3].id();
    cx.simulate_keystrokes("ctrl-,");
    cx.run_until_parked();
    let section = cx.debug_bounds("settings-section-prompts").unwrap();
    cx.simulate_click(section.center(), gpui::Modifiers::none());
    wait(cx, &view, listed);
    let conflict: &'static str = format!("prompt-conflict-{shadowed}").leak();
    assert!(cx.debug_bounds(conflict).is_some());
    let review = saved.workspace().prompts()[0].id();
    assert!(
        cx.debug_bounds(format!("prompt-conflict-{review}").leak())
            .is_none()
    );
}

/// Opening the library before any `/` asks the account for its commands, so a
/// shortcut one of them shares is flagged there.
#[gpui::test]
fn the_library_asks_for_commands_to_flag_a_shortcut_one_shares(cx: &mut TestAppContext) {
    let (_dir, view, cx) = open(Provider::Claude, cx);
    let id = view.update(cx, |view, cx| {
        view.state.update(cx, |state, cx| {
            state
                .change_prompts(
                    |store| {
                        store.add_prompt(keystone_core::PromptFields {
                            title: "Mine".into(),
                            body: "Mine.".into(),
                            shortcut: "fixture-review".into(),
                            project: None,
                        })
                    },
                    cx,
                )
                .unwrap()
                .value
        })
    });
    cx.simulate_keystrokes("ctrl-,");
    cx.run_until_parked();
    let section = cx.debug_bounds("settings-section-prompts").unwrap();
    cx.simulate_click(section.center(), gpui::Modifiers::none());
    wait(cx, &view, listed);
    cx.run_until_parked();
    assert!(
        cx.debug_bounds(format!("prompt-conflict-{id}").leak())
            .is_some()
    );
}

/// While the account's list loads, Enter and Tab never insert a prompt with a
/// shortcut, since a command may yet take its name; a click on its row still does.
#[gpui::test]
fn enter_waits_for_the_command_list_before_inserting_a_shortcut_prompt(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("project");
    std::fs::create_dir_all(&folder).unwrap();
    let silent = dir.path().join("silent");
    std::fs::write(&silent, "#!/bin/sh\nexec sleep 30\n").unwrap();
    std::fs::set_permissions(&silent, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut store = Store::open(dir.path().join("workspace.json")).unwrap();
    let id = store
        .create(
            Provider::Claude,
            &WorkingDirectory::new(&folder).unwrap(),
            1,
        )
        .unwrap()
        .value;
    store.select(id).unwrap();
    store
        .add_prompt(keystone_core::PromptFields {
            title: "Mine".into(),
            body: "Mine.".into(),
            shortcut: "fixture-review".into(),
            project: None,
        })
        .unwrap();
    store
        .add_prompt(keystone_core::PromptFields {
            title: "Log".into(),
            body: "Log.".into(),
            shortcut: "log".into(),
            project: None,
        })
        .unwrap();
    cx.update(|cx| {
        keystone_ui::init(cx);
        crate::bind_keys(cx);
    });
    cx.executor().allow_parking();
    let (view, cx) = cx.add_window_view(|window, cx| {
        let state = cx.new(|cx| {
            let mut state = AppState::new(store, cx);
            state.test_programs = Some((silent.clone(), silent.clone()));
            state
        });
        Keystone::new(state, false, window, cx)
    });
    view.update_in(cx, |this, window, cx| {
        this.composer.focus_handle(cx).focus(window, cx)
    });
    cx.simulate_input("/fixture-review");
    cx.run_until_parked();
    assert!(view.read_with(cx, |view, cx| {
        view.state.read(cx).commands(id, Provider::Claude).is_none()
    }));
    assert_eq!(shown(&view, cx)[0], ("/fixture-review".into(), "Saved"));
    cx.simulate_keystrokes("enter");
    cx.simulate_keystrokes("tab");
    cx.run_until_parked();
    assert_eq!(draft(&view, cx), "/fixture-review", "the keyboard waits");
    let row = cx.debug_bounds("slash-menu-list-row-0").unwrap();
    cx.simulate_click(row.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert_eq!(draft(&view, cx), "Mine.", "a click chooses it");

    // `/log` waits behind `/login` here; Enter acts on neither until the list is in.
    cx.simulate_keystrokes("ctrl-a backspace");
    cx.simulate_input("/log");
    cx.run_until_parked();
    assert_eq!(shown(&view, cx)[0].0, "/login");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(draft(&view, cx), "/log");
    assert_eq!(shown(&view, cx)[0].0, "/login", "no Terminal only notice");
}
