//! Settings → Integrations through the app, on fixture accounts in a temporary folder
//! and the scripted providers. The app's own environment is never consulted: tests
//! start with an empty [`keystone_agent::ConfigEnv`] and name the fixture folders.

use super::{Notice, copy_note, detail, note};
use crate::*;
use keystone_agent::{ConfigEnv, ItemStatus};
use keystone_core::integrations::Health;
use keystone_core::integrations::copy;
use std::os::unix::fs::PermissionsExt;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn skill(root: &Path, name: &str) {
    write(
        &root.join(name).join("SKILL.md"),
        &format!("---\nname: {name}\ndescription: Fixture {name}\n---\nBody.\n"),
    );
}

/// A wrapper that runs the scripted provider `name` with `env` set.
fn fake(dir: &Path, name: &str, env: &str) -> PathBuf {
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(name);
    assert!(
        binary.exists(),
        "build it with `cargo build -p keystone-agent --bins`"
    );
    let script = dir.join(format!("{name}-wrapper"));
    std::fs::write(
        &script,
        format!("#!/bin/sh\n{env} exec '{}' \"$@\"\n", binary.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    claude: PathBuf,
    codex: PathBuf,
    project: PathBuf,
}

/// One Claude and one Codex account with three MCP servers between them (one fails to
/// start), two skills and a plugin, each server given a secret.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_owned();
    let claude = root.join("claude-config");
    let codex = root.join("codex-home");
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    write(
        &claude.join(".claude.json"),
        r#"{"mcpServers": {"fixture-ok": {"type": "stdio", "command": "python3", "args": ["server.py", "--api-key", "fixture-secret-value"], "env": {"FIXTURE_TOKEN": "fixture-secret-value"}}, "fixture-remote": {"type": "http", "url": "https://u:fixture-secret-value@mcp.example/fixture-secret-value/mcp?key=fixture-secret-value"}}}"#,
    );
    write(
        &project.join(".mcp.json"),
        r#"{"mcpServers": {"fixture-broken": {"command": "false", "env": {"OTHER_TOKEN": "fixture-secret-value"}}}}"#,
    );
    write(
        &claude.join("settings.json"),
        r#"{"enabledPlugins": {"fixture-plugin@fixture-market": true}}"#,
    );
    skill(&claude.join("skills"), "fixture-skill");
    // The same skill name in two folders Codex reads at user scope.
    skill(&root.join(".agents/skills"), "fixture-codex-skill");
    write(
        &codex.join("config.toml"),
        "[mcp_servers.fixture-codex]\ncommand = \"python3\"\n\n[mcp_servers.fixture-codex.env]\nCODEX_TOKEN = \"fixture-secret-value\"\n",
    );
    skill(&codex.join("skills"), "fixture-codex-skill");
    Fixture {
        _dir: dir,
        root,
        claude,
        codex,
        project,
    }
}

fn open<'a>(
    fixture: &Fixture,
    cx: &'a mut gpui::TestAppContext,
) -> (Entity<Keystone>, &'a mut gpui::VisualTestContext) {
    let store = Store::open(fixture.root.join("workspace.json")).unwrap();
    cx.update(|cx| {
        keystone_ui::init(cx);
        bind_keys(cx);
    });
    cx.executor().allow_parking();
    let (view, cx) = cx.add_window_view(|window, cx| {
        let view = Keystone::new(cx.new(|cx| AppState::new(store, cx)), false, window, cx);
        view.focus.focus(window, cx);
        view
    });
    let claude = fake(
        &fixture.root,
        "keystone-fake-claude",
        &format!(
            "KEYSTONE_FAKE_CLAUDE_DELAY_MS=1 KEYSTONE_FAKE_MCP_CONFIG=1 CLAUDE_CONFIG_DIR='{}'",
            fixture.claude.display()
        ),
    );
    let codex = fake(
        &fixture.root,
        "keystone-fake-codex",
        &format!(
            "KEYSTONE_FAKE_CODEX_DELAY_MS=1 KEYSTONE_FAKE_MCP_CONFIG=1 CODEX_HOME='{}'",
            fixture.codex.display()
        ),
    );
    view.update(cx, |view, cx| {
        view.presentation.project =
            keystone_core::ProjectId::new(&fixture.project).map_err(|e| e.to_string());
        view.state.update(cx, |state, _| {
            state.config_env = ConfigEnv {
                claude_config_dir: Some(fixture.claude.clone()),
                codex_home: Some(fixture.codex.clone()),
                home: Some(fixture.root.clone()),
            };
            state.test_set_claude(Some(ClaudeConnection::new(
                Some(claude),
                state.test_wake().clone(),
            )));
            state.test_set_codex(Some(CodexConnection::new(
                Some(codex),
                "test",
                state.test_wake().clone(),
            )));
        });
    });
    (view, cx)
}

fn click(cx: &mut gpui::VisualTestContext, id: &'static str) {
    let bounds = cx
        .debug_bounds(id)
        .unwrap_or_else(|| panic!("{id} rendered"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
}

fn send(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext, session: u64, text: &str) {
    view.update_in(cx, |view, window, cx| {
        view.composer
            .update(cx, |input, cx| input.set_value(text.into(), cx));
        let draft = view.composer_draft(cx).unwrap();
        view.send_message(draft, window, cx);
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        cx.run_until_parked();
        let done = view.read_with(cx, |view, cx| {
            view.state
                .read(cx)
                .conversations()
                .get(&session)
                .is_some_and(|c| {
                    !c.running
                        && c.rows.iter().any(|row| {
                            matches!(row, Row::Item(item) if item.status == ItemStatus::Completed
                                && item.text.contains(text))
                        })
                })
        });
        if done {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "no reply to {text}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn health(
    view: &Entity<Keystone>,
    cx: &mut gpui::VisualTestContext,
    provider: Provider,
    name: &str,
) -> Option<Health> {
    view.read_with(cx, |view, cx| {
        let project = view.presentation.project.as_ref().unwrap().path();
        view.state
            .read(cx)
            .mcp_status(provider, None, project)
            .get(name)
            .map(|r| r.status.health)
    })
}

const ROWS: [&str; 8] = [
    "integration-claude-default-mcp-user-fixture-ok",
    "integration-claude-default-mcp-user-fixture-remote",
    "integration-claude-default-mcp-project-fixture-broken",
    "integration-codex-default-mcp-user-fixture-codex",
    "integration-claude-default-skill-user-fixture-skill",
    "integration-codex-default-skill-user-fixture-codex-skill",
    "integration-codex-default-skill-user-fixture-codex-skill-2",
    "integration-claude-default-plugin-user-fixture-plugin@fixture-market",
];

/// Every line the section shows for the items it read.
fn shown_text(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext) -> String {
    view.read_with(cx, |view, _| {
        let read = view.settings.as_ref().unwrap().integrations.read.as_ref();
        read.into_iter()
            .flatten()
            .flat_map(|account| account.inventory.iter().flat_map(|i| &i.items))
            .map(|item| format!("{} {} {:?}\n", item.name, detail(item), note(item)))
            .collect()
    })
}

#[gpui::test]
fn integrations_list_each_accounts_items_and_the_status_sessions_report(
    cx: &mut gpui::TestAppContext,
) {
    let fixture = fixture();
    let (view, vcx) = open(&fixture, cx);
    vcx.simulate_keystrokes("secondary-,");
    vcx.run_until_parked();
    click(vcx, "settings-section-integrations");
    assert!(vcx.debug_bounds("settings-integrations").is_some());
    for row in ROWS {
        assert!(vcx.debug_bounds(row).is_some(), "{row} listed");
    }
    assert!(
        vcx.debug_bounds("integrations-problems").is_none(),
        "every fixture file reads"
    );
    assert!(
        vcx.debug_bounds("integration-claude-default-mcp-user-fixture-ok-status-not-running")
            .is_some()
    );
    assert_eq!(health(&view, vcx, Provider::Claude, "fixture-ok"), None);
    let shown = shown_text(&view, vcx);
    assert!(shown.contains("python3 · 3 arguments"), "{shown}");
    assert!(shown.contains("https://mcp.example/… (http)"), "{shown}");
    assert!(!shown.contains("fixture-secret-value"), "{shown}");

    // A Claude session's `init` reports one server connected and one failed; a Codex
    // thread reports its server starting, then ready.
    click(vcx, "settings-back");
    click(vcx, "welcome-claude");
    send(&view, vcx, 1, "hello claude");
    assert_eq!(
        health(&view, vcx, Provider::Claude, "fixture-ok"),
        Some(Health::Connected)
    );
    assert_eq!(
        health(&view, vcx, Provider::Claude, "fixture-broken"),
        Some(Health::Failed)
    );
    vcx.dispatch_action(NewCodex);
    vcx.run_until_parked();
    send(&view, vcx, 2, "hello codex");
    assert_eq!(
        health(&view, vcx, Provider::Codex, "fixture-codex"),
        Some(Health::Connected)
    );
    assert_eq!(
        health(&view, vcx, Provider::Codex, "keystone"),
        None,
        "Keystone's own server is left out"
    );

    // A server no file declares, such as a plugin's, is listed as reported. The
    // session's next report replaces its last, keeping what it still names.
    view.update(vcx, |view, cx| {
        view.state.update(cx, |state, cx| {
            state.record_mcp_status(
                Provider::Claude,
                None,
                1,
                vec![
                    keystone_core::integrations::ServerStatus {
                        name: "plugin:fixture-plugin:server".into(),
                        health: Health::Failed,
                    },
                    keystone_core::integrations::ServerStatus {
                        name: "fixture-ok".into(),
                        health: Health::Connected,
                    },
                ],
                cx,
            )
        })
    });
    assert_eq!(health(&view, vcx, Provider::Claude, "fixture-broken"), None);
    vcx.simulate_keystrokes("secondary-,");
    vcx.run_until_parked();
    click(vcx, "settings-section-integrations");
    for row in ROWS {
        assert!(vcx.debug_bounds(row).is_some(), "{row} still listed");
    }
    for status in [
        "integration-claude-default-mcp-user-fixture-ok-status-connected",
        "integration-claude-default-mcp-project-fixture-broken-status-not-running",
        "integration-codex-default-mcp-user-fixture-codex-status-connected",
    ] {
        assert!(vcx.debug_bounds(status).is_some(), "{status} shown");
    }
    assert!(
        vcx.debug_bounds("integration-claude-default-mcp-reported-plugin:fixture-plugin:server")
            .is_some()
    );
    assert!(
        vcx.debug_bounds("integration-claude-default-mcp-reported-fixture-ok")
            .is_none(),
        "a declared server is not listed twice"
    );
}

#[gpui::test]
fn without_known_folders_nothing_is_read(cx: &mut gpui::TestAppContext) {
    let fixture = fixture();
    let (view, vcx) = open(&fixture, cx);
    view.update(vcx, |view, cx| {
        view.state
            .update(cx, |state, _| state.config_env = ConfigEnv::default())
    });
    vcx.simulate_keystrokes("secondary-,");
    vcx.run_until_parked();
    click(vcx, "settings-section-integrations");
    assert!(vcx.debug_bounds("integrations-problems").is_some());
    for row in ROWS {
        assert!(vcx.debug_bounds(row).is_none(), "{row} not read");
    }
}

/// A report belongs to the folder its session runs in and lasts while the session's
/// process does: servers a session in a subfolder loaded never show here, nor does a
/// report from a session that is not running. A server a stronger scope overrides shows
/// no status.
#[gpui::test]
fn reports_stay_with_their_project_and_skip_overridden_servers(cx: &mut gpui::TestAppContext) {
    let fixture = fixture();
    write(
        &fixture.project.join(".mcp.json"),
        r#"{"mcpServers": {"fixture-ok": {"command": "true"}}}"#,
    );
    let sub = fixture.project.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let (idle, in_sub) = {
        let mut store = Store::open(fixture.root.join("workspace.json")).unwrap();
        let mut create = |folder: &Path| {
            store
                .create(Provider::Claude, &WorkingDirectory::new(folder).unwrap(), 1)
                .unwrap()
                .value
        };
        (create(&fixture.project), create(&sub))
    };
    let (view, vcx) = open(&fixture, cx);
    send(&view, vcx, in_sub, "hello from sub");
    view.update(vcx, |view, cx| {
        view.state.update(cx, |state, cx| {
            state.record_mcp_status(
                Provider::Claude,
                None,
                idle,
                vec![keystone_core::integrations::ServerStatus {
                    name: "idle-only".into(),
                    health: Health::Connected,
                }],
                cx,
            )
        })
    });
    assert!(view.read_with(vcx, |view, cx| {
        let project = view.presentation.project.as_ref().unwrap().path();
        view.state
            .read(cx)
            .mcp_status(Provider::Claude, None, project)
            .is_empty()
    }));
    vcx.simulate_keystrokes("secondary-,");
    vcx.run_until_parked();
    click(vcx, "settings-section-integrations");
    assert!(
        vcx.debug_bounds("integration-claude-default-mcp-reported-idle-only")
            .is_none()
    );
    assert!(
        vcx.debug_bounds("integration-claude-default-mcp-project-fixture-ok-status-not-running")
            .is_some()
    );
    assert!(
        vcx.debug_bounds("integration-claude-default-mcp-user-fixture-ok-status-none")
            .is_some(),
        "the user declaration the project one overrides has no status"
    );
    let shown = shown_text(&view, vcx);
    assert!(
        shown.contains(
            "fixture-ok python3 · 3 arguments · env FIXTURE_TOKEN Some(\"Overridden by Project scope\")"
        ),
        "{shown}"
    );
}

/// Opens Integrations with the scripted CLIs as the ones config changes run, recording
/// each call in `receipt`.
fn open_for_changes<'a>(
    fixture: &Fixture,
    cx: &'a mut gpui::TestAppContext,
) -> (Entity<Keystone>, &'a mut gpui::VisualTestContext, PathBuf) {
    let receipt = fixture.root.join("cli-receipt.jsonl");
    let env = |variable: &str, home: &Path| {
        format!(
            "KEYSTONE_FAKE_ENV_RECEIPT='{}' {variable}='{}'",
            receipt.display(),
            home.display()
        )
    };
    let (view, vcx) = open(fixture, cx);
    // After `open`, whose wrappers have the same names.
    let claude = fake(
        &fixture.root,
        "keystone-fake-claude",
        &env("CLAUDE_CONFIG_DIR", &fixture.claude),
    );
    let codex = fake(
        &fixture.root,
        "keystone-fake-codex",
        &env("CODEX_HOME", &fixture.codex),
    );
    view.update(vcx, |view, cx| {
        view.state
            .update(cx, |state, _| state.test_programs = Some((codex, claude)))
    });
    vcx.simulate_keystrokes("secondary-,");
    vcx.run_until_parked();
    click(vcx, "settings-section-integrations");
    (view, vcx, receipt)
}

/// Runs the page until no change or read is under way.
fn settle(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        cx.run_until_parked();
        let busy = view.read_with(cx, |view, _| {
            let page = &view.settings.as_ref().unwrap().integrations;
            page.applying || page.reading
        });
        if !busy {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "the change finished");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn notice(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext) -> Option<Notice> {
    view.read_with(cx, |view, _| {
        view.settings.as_ref().unwrap().integrations.notice.clone()
    })
}

fn set(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext, field: &str, value: &str) {
    view.update(cx, |view, cx| {
        let page = &view.settings.as_ref().unwrap().integrations;
        let input = match field {
            "name" => &page.name,
            "target" => &page.target,
            "args" => &page.args,
            "env" => &page.env,
            _ => &page.folder,
        }
        .clone();
        input.update(cx, |input, cx| input.set_value(value.into(), cx));
    });
}

fn value(view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext, field: &str) -> String {
    view.read_with(cx, |view, cx| {
        let page = &view.settings.as_ref().unwrap().integrations;
        match field {
            "target" => &page.target,
            "args" => &page.args,
            _ => &page.env,
        }
        .read(cx)
        .value()
        .to_owned()
    })
}

const CODEX_SWITCH: &str = "integration-codex-default-mcp-user-fixture-codex-switch";

#[gpui::test]
fn switching_a_codex_server_off_writes_only_its_table_and_tells_running_sessions(
    cx: &mut gpui::TestAppContext,
) {
    let fixture = fixture();
    let config = fixture.codex.join("config.toml");
    let before = "# mine\n[mcp_servers.fixture-codex]\n# how it starts\ncommand = \"python3\" # kept\n\n[mcp_servers.fixture-codex.env]\nCODEX_TOKEN = \"fixture-secret-value\"\n";
    write(&config, before);
    let (view, vcx, _) = open_for_changes(&fixture, cx);
    click(vcx, CODEX_SWITCH);
    settle(&view, vcx);
    assert_eq!(
        notice(&view, vcx),
        None,
        "no session runs, so nothing to say"
    );
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        before.replace("# kept\n", "# kept\nenabled = false\n")
    );
    assert!(
        vcx.debug_bounds("integration-codex-default-mcp-user-fixture-codex-status-none")
            .is_some()
    );

    click(vcx, "settings-back");
    vcx.dispatch_action(NewCodex);
    vcx.run_until_parked();
    send(&view, vcx, 1, "hello codex");
    vcx.simulate_keystrokes("secondary-,");
    vcx.run_until_parked();
    click(vcx, "settings-section-integrations");
    settle(&view, vcx);
    click(vcx, CODEX_SWITCH);
    settle(&view, vcx);
    assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
    assert_eq!(notice(&view, vcx), Some(Notice::NextStart));
    assert!(vcx.debug_bounds("integrations-next-start").is_some());
}

#[gpui::test]
fn an_outside_edit_shows_the_conflict_and_writes_nothing(cx: &mut gpui::TestAppContext) {
    let fixture = fixture();
    let config = fixture.codex.join("config.toml");
    let (view, vcx, receipt) = open_for_changes(&fixture, cx);
    settle(&view, vcx);
    let edited = format!(
        "{}# edited elsewhere\n",
        std::fs::read_to_string(&config).unwrap()
    );
    write(&config, &edited);
    click(vcx, CODEX_SWITCH);
    settle(&view, vcx);
    assert_eq!(notice(&view, vcx), Some(Notice::Conflict));
    assert!(vcx.debug_bounds("integrations-conflict").is_some());
    assert_eq!(std::fs::read_to_string(&config).unwrap(), edited);
    click(
        vcx,
        "integration-codex-default-mcp-user-fixture-codex-remove",
    );
    settle(&view, vcx);
    assert_eq!(std::fs::read_to_string(&config).unwrap(), edited);
    let ran = std::fs::read_to_string(&receipt).unwrap_or_default();
    assert!(!ran.contains("\"mcp\""), "no CLI ran: {ran}");

    click(vcx, "integrations-review");
    settle(&view, vcx);
    assert_eq!(notice(&view, vcx), None);
    click(vcx, CODEX_SWITCH);
    settle(&view, vcx);
    assert_eq!(notice(&view, vcx), None);
    assert!(
        std::fs::read_to_string(&config)
            .unwrap()
            .contains("enabled = false")
    );
}

#[gpui::test]
fn adding_a_claude_server_runs_claude_mcp_add_json(cx: &mut gpui::TestAppContext) {
    let fixture = fixture();
    let (view, vcx, receipt) = open_for_changes(&fixture, cx);
    settle(&view, vcx);
    click(vcx, "integrations-add-mcp");
    assert!(vcx.debug_bounds("integrations-server-form").is_some());
    set(&view, vcx, "name", "added");
    set(&view, vcx, "target", "npx");
    set(&view, vcx, "args", "-y 'my server'");
    set(&view, vcx, "env", "API_KEY=k");
    click(vcx, "integrations-form-submit");
    settle(&view, vcx);
    assert_eq!(notice(&view, vcx), None);
    assert!(vcx.debug_bounds("integrations-server-form").is_none());
    assert!(
        vcx.debug_bounds("integration-claude-default-mcp-user-added")
            .is_some()
    );
    let calls: Vec<serde_json::Value> = std::fs::read_to_string(&receipt)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|call| call["argv"][0] == "mcp")
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0]["argv"],
        serde_json::json!([
            "mcp",
            "add-json",
            "-s",
            "user",
            "added",
            r#"{"args":["-y","my server"],"command":"npx","env":{"API_KEY":"k"},"type":"stdio"}"#
        ])
    );
    assert_eq!(
        calls[0]["CLAUDE_CONFIG_DIR"],
        fixture.claude.to_str().unwrap()
    );

    // The same name again is the CLI's own refusal.
    click(vcx, "integrations-add-mcp");
    set(&view, vcx, "name", "added");
    set(&view, vcx, "target", "npx");
    click(vcx, "integrations-form-submit");
    settle(&view, vcx);
    assert_eq!(
        notice(&view, vcx),
        Some(Notice::Failed(
            "MCP server added already exists in user config".into()
        ))
    );
    assert!(
        vcx.debug_bounds("integrations-server-form").is_some(),
        "the form stays"
    );
}

#[gpui::test]
fn editing_a_server_keeps_values_the_form_hides(cx: &mut gpui::TestAppContext) {
    let fixture = fixture();
    let (view, vcx, _) = open_for_changes(&fixture, cx);
    settle(&view, vcx);
    click(vcx, "integration-claude-default-mcp-user-fixture-ok-edit");
    assert_eq!(value(&view, vcx, "target"), "python3");
    assert_eq!(
        value(&view, vcx, "args"),
        super::KEPT,
        "arguments can hold a token"
    );
    assert_eq!(
        value(&view, vcx, "env"),
        format!("FIXTURE_TOKEN={}", super::KEPT)
    );
    set(&view, vcx, "args", "server.py --port 9");
    click(vcx, "integrations-form-submit");
    settle(&view, vcx);
    assert_eq!(notice(&view, vcx), None);
    let global: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(fixture.claude.join(".claude.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        global["mcpServers"]["fixture-ok"],
        serde_json::json!({"type": "stdio", "command": "python3", "args": ["server.py", "--port", "9"], "env": {"FIXTURE_TOKEN": "fixture-secret-value"}})
    );
}

#[gpui::test]
fn the_default_claude_accounts_switch_is_off_limits_and_a_skill_folder_comes_and_goes(
    cx: &mut gpui::TestAppContext,
) {
    let fixture = fixture();
    let global = fixture.claude.join(".claude.json");
    let before = std::fs::read_to_string(&global).unwrap();
    let (view, vcx, _) = open_for_changes(&fixture, cx);
    settle(&view, vcx);
    assert!(
        vcx.debug_bounds("integration-claude-default-mcp-user-fixture-ok-switch-claude")
            .is_some(),
        "the switch only Claude can change is shown off limits"
    );
    click(vcx, "integration-claude-default-mcp-user-fixture-ok-switch");
    settle(&view, vcx);
    assert_eq!(notice(&view, vcx), None);
    assert_eq!(std::fs::read_to_string(&global).unwrap(), before);

    let folder = fixture.root.join("new-skill");
    skill(&fixture.root, "new-skill");
    click(vcx, "integrations-add-skill");
    set(&view, vcx, "folder", folder.to_str().unwrap());
    click(vcx, "integrations-form-submit");
    settle(&view, vcx);
    assert_eq!(notice(&view, vcx), None);
    let row = "integration-claude-default-skill-user-new-skill";
    assert!(vcx.debug_bounds(row).is_some());
    assert!(fixture.claude.join("skills/new-skill/SKILL.md").is_file());
    click(
        vcx,
        "integration-claude-default-skill-user-new-skill-remove",
    );
    settle(&view, vcx);
    assert!(vcx.debug_bounds(row).is_none());
    assert!(!fixture.claude.join("skills/new-skill").exists());
    assert!(folder.join("SKILL.md").is_file());
}

/// The text the open copy panel previews, and the secrets it offers.
fn copy_preview(
    view: &Entity<Keystone>,
    cx: &mut gpui::VisualTestContext,
) -> (String, Vec<String>) {
    view.read_with(cx, |view, _| {
        match &view.settings.as_ref().unwrap().integrations.form {
            Some(super::Form {
                kind: super::FormKind::Copy(copy),
                ..
            }) => {
                let (asked, preview) = copy.preview.as_ref().expect("the preview is ready");
                assert_eq!(*asked, copy.asked, "the preview is current");
                let preview = preview.as_ref().unwrap();
                (preview.text.clone(), preview.secrets.clone())
            }
            _ => panic!("the copy panel is open"),
        }
    })
}

/// What each row of `name` says beside it, by row.
fn notes(
    view: &Entity<Keystone>,
    cx: &mut gpui::VisualTestContext,
    name: &str,
) -> Vec<Option<String>> {
    view.read_with(cx, |view, _| {
        let page = &view.settings.as_ref().unwrap().integrations;
        let read = page.read.as_ref().unwrap();
        read.iter()
            .flat_map(|account| account.inventory.iter().flat_map(|i| &i.items))
            .filter(|item| item.name == name)
            .map(|item| copy_note(item, &page.copies, read).or_else(|| note(item)))
            .collect()
    })
}

#[gpui::test]
fn a_claude_server_and_skill_copy_to_codex_as_previewed_and_a_later_edit_is_flagged(
    cx: &mut gpui::TestAppContext,
) {
    let fixture = fixture();
    let config = fixture.codex.join("config.toml");
    let before = std::fs::read_to_string(&config).unwrap();
    let (view, vcx, receipt) = open_for_changes(&fixture, cx);
    settle(&view, vcx);
    let plugin = "integration-claude-default-plugin-user-fixture-plugin@fixture-market";
    assert!(
        vcx.debug_bounds(format!("{plugin}-copy").leak()).is_none(),
        "a plugin offers no copy"
    );
    assert_eq!(
        notes(&view, vcx, "fixture-plugin@fixture-market"),
        [Some("Claude only".into())]
    );

    click(vcx, "integration-claude-default-mcp-user-fixture-ok-copy");
    assert!(vcx.debug_bounds("integrations-copy-form").is_some());
    let (text, secrets) = copy_preview(&view, vcx);
    assert_eq!(secrets, ["FIXTURE_TOKEN", "--api-key"]);
    assert!(!text.contains("fixture-secret-value"), "{text}");
    assert!(
        !text.contains("FIXTURE_TOKEN"),
        "left out until included: {text}"
    );
    click(vcx, "integrations-copy-secret-FIXTURE_TOKEN");
    let (text, _) = copy_preview(&view, vcx);
    assert!(
        text.contains(&format!("FIXTURE_TOKEN = \"{}\"", copy::MASK)),
        "{text}"
    );
    assert!(!text.contains("fixture-secret-value"), "{text}");
    click(vcx, "integrations-form-submit");
    settle(&view, vcx);
    assert_eq!(notice(&view, vcx), None);
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        format!(
            "{before}\n{}",
            text.replace(copy::MASK, "fixture-secret-value")
        ),
        "config.toml changes exactly as previewed"
    );
    let ran = std::fs::read_to_string(&receipt).unwrap_or_default();
    assert!(!ran.contains("\"mcp\""), "Codex copies need no CLI: {ran}");
    assert!(
        vcx.debug_bounds("integration-codex-default-mcp-user-fixture-ok")
            .is_some()
    );
    assert_eq!(
        notes(&view, vcx, "fixture-ok"),
        [None, Some("Needs --api-key".into())],
        "the copy names the secret left out"
    );

    click(
        vcx,
        "integration-claude-default-skill-user-fixture-skill-copy",
    );
    assert_eq!(copy_preview(&view, vcx).0, "SKILL.md");
    click(vcx, "integrations-form-submit");
    settle(&view, vcx);
    assert_eq!(notice(&view, vcx), None);
    assert_eq!(
        std::fs::read(fixture.codex.join("skills/fixture-skill/SKILL.md")).unwrap(),
        std::fs::read(fixture.claude.join("skills/fixture-skill/SKILL.md")).unwrap()
    );
    assert_eq!(notes(&view, vcx, "fixture-skill"), [None, None]);

    let global = fixture.claude.join(".claude.json");
    write(
        &global,
        &std::fs::read_to_string(&global)
            .unwrap()
            .replace("server.py", "server2.py"),
    );
    write(
        &fixture.claude.join("skills/fixture-skill/SKILL.md"),
        "---\nname: fixture-skill\ndescription: Edited\n---\n",
    );
    click(vcx, "integrations-refresh");
    settle(&view, vcx);
    let out_of_sync = |with: &str| Some(format!("Out of sync with {with} · Default"));
    assert_eq!(
        notes(&view, vcx, "fixture-ok"),
        [out_of_sync("Codex"), out_of_sync("Claude Code")]
    );
    assert_eq!(
        notes(&view, vcx, "fixture-skill"),
        [out_of_sync("Codex"), out_of_sync("Claude Code")]
    );
    assert!(
        vcx.debug_bounds("integration-claude-default-mcp-user-fixture-ok-note")
            .is_some()
    );
}

#[gpui::test]
fn a_copy_from_codex_to_claude_runs_claude_mcp_add_json(cx: &mut gpui::TestAppContext) {
    let fixture = fixture();
    let (view, vcx, receipt) = open_for_changes(&fixture, cx);
    settle(&view, vcx);
    click(vcx, "integration-codex-default-mcp-user-fixture-codex-copy");
    click(vcx, "integrations-copy-secret-CODEX_TOKEN");
    click(vcx, "integrations-form-submit");
    settle(&view, vcx);
    assert_eq!(notice(&view, vcx), None);
    let calls: Vec<serde_json::Value> = std::fs::read_to_string(&receipt)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|call| call["argv"][0] == "mcp")
        .collect();
    assert_eq!(
        calls
            .iter()
            .map(|call| call["argv"].clone())
            .collect::<Vec<_>>(),
        [serde_json::json!([
            "mcp",
            "add-json",
            "-s",
            "user",
            "fixture-codex",
            r#"{"command":"python3","env":{"CODEX_TOKEN":"fixture-secret-value"},"type":"stdio"}"#
        ])]
    );
    assert!(
        vcx.debug_bounds("integration-claude-default-mcp-user-fixture-codex")
            .is_some()
    );
    assert_eq!(notes(&view, vcx, "fixture-codex"), [None, None]);
}

#[gpui::test]
fn the_copy_panel_closes_when_the_accounts_move_and_a_removed_copy_is_forgotten(
    cx: &mut gpui::TestAppContext,
) {
    let fixture = fixture();
    let (view, vcx, _) = open_for_changes(&fixture, cx);
    settle(&view, vcx);
    click(
        vcx,
        "integration-claude-default-skill-user-fixture-skill-copy",
    );
    click(vcx, "integrations-form-submit");
    settle(&view, vcx);
    let remembered = |view: &Entity<Keystone>, cx: &mut gpui::VisualTestContext| {
        view.read_with(cx, |view, cx| {
            view.state
                .read(cx)
                .store()
                .database()
                .copies()
                .unwrap()
                .len()
        })
    };
    assert_eq!(remembered(&view, vcx), 1);

    click(vcx, "integration-claude-default-mcp-user-fixture-ok-copy");
    assert!(vcx.debug_bounds("integrations-copy-form").is_some());
    let work = fixture.root.join("codex-work");
    std::fs::create_dir_all(&work).unwrap();
    view.update(vcx, |view, cx| {
        view.state.update(cx, |state, cx| {
            state
                .add_account(Provider::Codex, "Work", work.to_str().unwrap(), cx)
                .unwrap();
        })
    });
    click(vcx, "integrations-refresh");
    settle(&view, vcx);
    assert!(
        vcx.debug_bounds("integrations-copy-form").is_none(),
        "a copy aimed by position must not land on another account"
    );

    click(
        vcx,
        "integration-codex-default-skill-user-fixture-skill-remove",
    );
    settle(&view, vcx);
    assert_eq!(
        remembered(&view, vcx),
        0,
        "a copy that is gone is forgotten"
    );
}
