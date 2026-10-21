//! Settings → Integrations through the app, on fixture accounts in a temporary folder
//! and the scripted providers. The app's own environment is never consulted: tests
//! start with an empty [`keystone_agent::ConfigEnv`] and name the fixture folders.

use super::{detail, note};
use crate::*;
use keystone_agent::{ConfigEnv, ItemStatus};
use keystone_core::integrations::Health;
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
    vcx.simulate_keystrokes("ctrl-,");
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
    vcx.simulate_keystrokes("ctrl-,");
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
    vcx.simulate_keystrokes("ctrl-,");
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
    vcx.simulate_keystrokes("ctrl-,");
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
