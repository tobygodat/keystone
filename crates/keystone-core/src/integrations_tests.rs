use super::*;
use std::fs;

const SECRETS: [&str; 6] = [
    "fixture-arg-secret",
    "fixture-env-secret",
    "fixture-header-secret",
    "fixture-local-secret",
    "fixture-codex-secret",
    "fixture-url-secret",
];

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn skill(root: &Path, name: &str, description: &str) {
    write(
        &root.join(name).join("SKILL.md"),
        &format!("---\nname: {name}\ndescription: {description}\n---\nBody.\n"),
    );
}

fn summary(inventory: &Inventory) -> Vec<(Kind, String, Scope, State, PathBuf)> {
    inventory
        .items
        .iter()
        .map(|i| (i.kind(), i.name.clone(), i.scope, i.state, i.source.clone()))
        .collect()
}

fn server<'a>(inventory: &'a Inventory, name: &str, scope: Scope) -> &'a McpServer {
    inventory
        .items
        .iter()
        .find_map(|item| match &item.detail {
            Detail::McpServer(server) if item.name == name && item.scope == scope => Some(server),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{name} listed"))
}

fn overrides(inventory: &Inventory) -> Vec<(String, Scope, Scope)> {
    inventory
        .items
        .iter()
        .filter_map(|i| Some((i.name.clone(), i.scope, i.overridden_by?)))
        .collect()
}

fn assert_no_secret(inventory: &Inventory) {
    let shown = format!("{inventory:?}");
    for secret in SECRETS {
        assert!(!shown.contains(secret), "{secret} leaked: {shown}");
    }
}

/// A Claude account laid out as Claude Code 2.1.301 writes it with `claude mcp add`
/// at each scope and `claude plugin install` at user and local scope.
fn claude_fixture(root: &Path) -> (Location, PathBuf) {
    let config = root.join("claude-config");
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    let global = config.join(".claude.json");
    write(
        &global,
        &serde_json::json!({
            "mcpServers": {
                "fixture-user": {"type": "stdio", "command": "python3", "args": ["server.py", "--api-key", "fixture-arg-secret", "sh -c 'TOKEN=fixture-arg-secret'"], "env": {"FIXTURE_TOKEN": "fixture-env-secret"}},
                "fixture-off": {"command": "true"},
                "fixture-project": {"command": "true"},
            },
            "projects": {
                project.to_str().unwrap(): {
                    "mcpServers": {
                        "fixture-local": {"type": "http", "url": "https://user:fixture-url-secret@mcp.example/mcp?key=fixture-url-secret#x", "headers": {"Authorization": "Bearer fixture-local-secret"}}
                    },
                    "disabledMcpServers": ["fixture-off"],
                    "disabledMcpjsonServers": ["fixture-denied"]
                },
                "/elsewhere": {"mcpServers": {"other-project": {"command": "true"}}}
            }
        })
        .to_string(),
    );
    write(
        &project.join(".mcp.json"),
        r#"{"mcpServers": {"fixture-project": {"type": "sse", "url": "http://127.0.0.1:9/fixture-url-secret/sse", "headers": {"X-Api-Key": "fixture-header-secret"}}, "fixture-denied": {"command": "false"}}}"#,
    );
    write(
        &config.join("settings.json"),
        r#"{"enabledPlugins": {"fixture-plugin@fixture-market": false}}"#,
    );
    write(
        &project.join(".claude/settings.local.json"),
        r#"{"enabledPlugins": {"fixture-plugin@fixture-market": true}}"#,
    );
    write(
        &config.join("plugins/installed_plugins.json"),
        &serde_json::json!({"version": 2, "plugins": {
            "fixture-plugin@fixture-market": [
                {"scope": "user", "version": "1.0.0", "installPath": config.join("plugins/cache/fixture-market/fixture-plugin/1.0.0")},
                {"scope": "local", "version": "1.1.0", "projectPath": project, "installPath": config.join("plugins/cache/fixture-market/fixture-plugin/1.1.0")},
                {"scope": "local", "version": "9.9.9", "projectPath": "/elsewhere"}
            ],
            "quiet-plugin@fixture-market": [
                {"scope": "user", "version": "2.0.0"},
                {"scope": "project", "version": "3.0.0"}
            ]
        }})
        .to_string(),
    );
    skill(
        &config.join("skills"),
        "fixture-skill",
        "Fixture user skill",
    );
    let plugins = config.join("plugins/cache/fixture-market/fixture-plugin");
    skill(&plugins.join("1.0.0/skills"), "old-skill", "Older version");
    skill(
        &plugins.join("1.1.0/skills"),
        "fixture-plugin-skill",
        "Plugin skill",
    );
    skill(
        &project.join(".claude/skills"),
        "fixture-project-skill",
        "Fixture project skill",
    );
    (
        Location::Claude {
            config_dir: config,
            global_file: global,
        },
        project,
    )
}

#[test]
fn claude_lists_every_scope_with_its_source_and_state() {
    let root = tempfile::tempdir().unwrap();
    let (location, project) = claude_fixture(root.path());
    let inventory = read(&AccountId::new("claude-work"), &location, &project);
    let config = root.path().join("claude-config");
    let global = config.join(".claude.json");
    use Kind::*;
    use Scope::*;
    use State::*;
    assert_eq!(
        summary(&inventory),
        [
            (
                McpServer,
                "fixture-denied".into(),
                Project,
                Off,
                project.join(".mcp.json")
            ),
            (McpServer, "fixture-local".into(), Local, On, global.clone()),
            (McpServer, "fixture-off".into(), User, Off, global.clone()),
            (
                McpServer,
                "fixture-project".into(),
                User,
                On,
                global.clone()
            ),
            (
                McpServer,
                "fixture-project".into(),
                Project,
                On,
                project.join(".mcp.json")
            ),
            (McpServer, "fixture-user".into(), User, On, global.clone()),
            (
                Skill,
                "fixture-plugin:fixture-plugin-skill".into(),
                Local,
                On,
                config.join(
                    "plugins/cache/fixture-market/fixture-plugin/1.1.0/skills/fixture-plugin-skill"
                )
            ),
            (
                Skill,
                "fixture-project-skill".into(),
                Project,
                On,
                project.join(".claude/skills/fixture-project-skill")
            ),
            (
                Skill,
                "fixture-skill".into(),
                User,
                On,
                config.join("skills/fixture-skill")
            ),
            (
                Plugin,
                "fixture-plugin@fixture-market".into(),
                User,
                Off,
                config.join("settings.json")
            ),
            (
                Plugin,
                "fixture-plugin@fixture-market".into(),
                Local,
                On,
                project.join(".claude/settings.local.json")
            ),
            (
                Plugin,
                "quiet-plugin@fixture-market".into(),
                User,
                Off,
                config.join("plugins/installed_plugins.json")
            ),
        ]
    );
    assert!(inventory.problems.is_empty(), "{:?}", inventory.problems);
    assert!(
        inventory
            .items
            .iter()
            .all(|i| i.provider == Provider::Claude && i.account.as_str() == "claude-work")
    );
    let versions: Vec<_> = inventory
        .items
        .iter()
        .filter_map(|i| match &i.detail {
            Detail::Plugin { version } => Some((i.scope, version.as_deref())),
            _ => None,
        })
        .collect();
    assert_eq!(
        versions,
        [
            (User, Some("1.0.0")),
            (Local, Some("1.1.0")),
            (User, Some("2.0.0"))
        ]
    );
    assert_eq!(
        server(&inventory, "fixture-user", User),
        &super::McpServer {
            transport: Transport::Stdio {
                command: "python3".into(),
                args: 4,
            },
            env: vec!["FIXTURE_TOKEN".into()],
            headers: vec![],
        }
    );
    assert_eq!(
        server(&inventory, "fixture-local", Local).transport,
        Transport::Remote {
            kind: "http".into(),
            url: "https://mcp.example/\u{2026}".into()
        }
    );
    let project_server = server(&inventory, "fixture-project", Project);
    assert_eq!(project_server.headers, ["X-Api-Key"]);
    assert_eq!(
        project_server.transport,
        Transport::Remote {
            kind: "sse".into(),
            url: "http://127.0.0.1:9/\u{2026}".into()
        }
    );
    let skills: Vec<_> = inventory
        .items
        .iter()
        .filter(|i| i.kind() == Skill)
        .map(|i| i.detail.clone())
        .collect();
    assert_eq!(
        skills[..2],
        [
            Detail::Skill {
                description: "Plugin skill".into(),
                plugin: Some("fixture-plugin@fixture-market".into()),
            },
            Detail::Skill {
                description: "Fixture project skill".into(),
                plugin: None,
            }
        ]
    );
    assert_eq!(
        overrides(&inventory),
        [
            ("fixture-project".into(), User, Project),
            ("fixture-plugin@fixture-market".into(), User, Local),
        ]
    );
    assert_no_secret(&inventory);
}

/// A Codex home laid out as Codex 0.163.0 writes it with `codex mcp add` and
/// `codex plugin add`, with a skill turned off through `[[skills.config]]`.
fn codex_fixture(root: &Path, trusted: bool) -> (Location, PathBuf) {
    let home = root.join("codex-home");
    let user = root.join("user");
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    let off_skill = home.join("skills/fixture-off-skill/SKILL.md");
    let trust = if trusted {
        format!(
            "[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
            project.display()
        )
    } else {
        String::new()
    };
    write(
        &home.join("config.toml"),
        &format!(
            r#"# The user's own comment
model = "gpt"

[mcp_servers.fixture-codex]
command = "python3"
args = ["server.py", "--token=fixture-arg-secret"]
env_vars = ["FORWARDED"]

[mcp_servers.fixture-codex.env]
FIXTURE_SECRET = "fixture-codex-secret"

[mcp_servers.fixture-http]
url = "https://mcp.example/mcp?token=fixture-url-secret"
bearer_token_env_var = "FIXTURE_BEARER"
enabled = false

[mcp_servers.fixture-http.http_headers]
X-Team = "fixture-header-secret"

[mcp_servers.fixture-repo]
command = "true"

[plugins."fixture-plugin@fixture-market"]
enabled = true

{trust}
[[skills.config]]
path = "{}"
enabled = false
"#,
            off_skill.display()
        ),
    );
    write(
        &project.join(".codex/config.toml"),
        "[mcp_servers.fixture-repo]\ncommand = \"true\"\n",
    );
    skill(
        &home.join("skills"),
        "fixture-codex-skill",
        "Fixture Codex skill",
    );
    skill(&home.join("skills"), "fixture-off-skill", "Off skill");
    skill(
        &home.join("plugins/cache/fixture-market/fixture-plugin/1.0.0/skills"),
        "fixture-plugin-skill",
        "Codex plugin skill",
    );
    skill(&home.join("skills/.system"), "bundled", "Codex's own");
    skill(
        &user.join(".agents/skills"),
        "fixture-agents-skill",
        "Agents skill",
    );
    skill(
        &project.join(".agents/skills"),
        "fixture-repo-skill",
        "Repo skill",
    );
    (
        Location::Codex {
            home,
            user_home: Some(user),
        },
        project,
    )
}

#[test]
fn codex_lists_servers_plugins_and_skills_and_honours_trust() {
    let root = tempfile::tempdir().unwrap();
    let (location, project) = codex_fixture(root.path(), false);
    let inventory = read(&AccountId::new(AccountId::DEFAULT), &location, &project);
    let home = root.path().join("codex-home");
    let config = home.join("config.toml");
    use Kind::*;
    use Scope::*;
    use State::*;
    assert_eq!(
        summary(&inventory),
        [
            (McpServer, "fixture-codex".into(), User, On, config.clone()),
            (McpServer, "fixture-http".into(), User, Off, config.clone()),
            (McpServer, "fixture-repo".into(), User, On, config.clone()),
            (
                McpServer,
                "fixture-repo".into(),
                Project,
                Untrusted,
                project.join(".codex/config.toml")
            ),
            (
                Skill,
                "fixture-agents-skill".into(),
                User,
                On,
                root.path().join("user/.agents/skills/fixture-agents-skill")
            ),
            (
                Skill,
                "fixture-codex-skill".into(),
                User,
                On,
                home.join("skills/fixture-codex-skill")
            ),
            (
                Skill,
                "fixture-off-skill".into(),
                User,
                Off,
                home.join("skills/fixture-off-skill")
            ),
            (
                Skill,
                "fixture-plugin:fixture-plugin-skill".into(),
                User,
                On,
                home.join(
                    "plugins/cache/fixture-market/fixture-plugin/1.0.0/skills/fixture-plugin-skill"
                )
            ),
            (
                Skill,
                "fixture-repo-skill".into(),
                Project,
                On,
                project.join(".agents/skills/fixture-repo-skill")
            ),
            (
                Plugin,
                "fixture-plugin@fixture-market".into(),
                User,
                On,
                config.clone()
            ),
        ]
    );
    assert!(inventory.problems.is_empty(), "{:?}", inventory.problems);
    assert_eq!(overrides(&inventory), []);
    let codex = server(&inventory, "fixture-codex", User);
    assert_eq!(codex.env, ["FIXTURE_SECRET", "FORWARDED"]);
    assert_eq!(
        codex.transport,
        Transport::Stdio {
            command: "python3".into(),
            args: 2
        }
    );
    let http = server(&inventory, "fixture-http", User);
    assert_eq!(
        http.transport,
        Transport::Remote {
            kind: "http".into(),
            url: "https://mcp.example/\u{2026}".into()
        }
    );
    assert_eq!(http.headers, ["X-Team", "Authorization"]);
    assert_no_secret(&inventory);

    let root = tempfile::tempdir().unwrap();
    let (location, project) = codex_fixture(root.path(), true);
    let inventory = read(&AccountId::new(AccountId::DEFAULT), &location, &project);
    let repo = inventory
        .items
        .iter()
        .find(|i| i.name == "fixture-repo" && i.scope == Project)
        .unwrap();
    assert_eq!(repo.state, On);
    assert_eq!(
        overrides(&inventory),
        [("fixture-repo".into(), User, Project)]
    );
}

/// Claude keys a project by its git root and reads `.mcp.json` above the folder it
/// runs in, so a subfolder sees the root's local servers and both `.mcp.json` files,
/// the nearer one winning.
#[test]
fn a_subfolder_reads_its_git_root() {
    let root = tempfile::tempdir().unwrap();
    let (location, repo) = claude_fixture(root.path());
    fs::create_dir_all(repo.join(".git")).unwrap();
    let sub = repo.join("sub");
    write(
        &sub.join(".mcp.json"),
        r#"{"mcpServers": {"fixture-project": {"command": "true"}}}"#,
    );
    let inventory = read(&AccountId::new("claude-a"), &location, &sub);
    let servers: Vec<_> = inventory
        .items
        .iter()
        .filter(|i| i.kind() == Kind::McpServer)
        .map(|i| (i.name.as_str(), i.scope, i.source.clone()))
        .collect();
    let global = root.path().join("claude-config/.claude.json");
    use Scope::*;
    assert_eq!(
        servers,
        [
            ("fixture-denied", Project, repo.join(".mcp.json")),
            ("fixture-local", Local, global.clone()),
            ("fixture-off", User, global.clone()),
            ("fixture-project", User, global.clone()),
            ("fixture-project", Project, repo.join(".mcp.json")),
            ("fixture-project", Project, sub.join(".mcp.json")),
            ("fixture-user", User, global),
        ]
    );
    let shadowed: Vec<_> = inventory
        .items
        .iter()
        .filter(|i| i.name == "fixture-project")
        .map(|i| (i.source.clone(), i.overridden_by))
        .collect();
    assert!(shadowed.contains(&(repo.join(".mcp.json"), Some(Project))));
    assert!(shadowed.contains(&(sub.join(".mcp.json"), None)));
}

#[test]
fn broken_files_are_problems_without_their_contents_and_the_rest_still_reads() {
    let root = tempfile::tempdir().unwrap();
    let (location, project) = codex_fixture(root.path(), true);
    write(
        &project.join(".codex/config.toml"),
        "[mcp_servers.x]\nenv = { TOKEN = \"fixture-codex-secret\" \n",
    );
    let inventory = read(&AccountId::new("codex-a"), &location, &project);
    assert_eq!(inventory.problems.len(), 1);
    assert_eq!(
        inventory.problems[0].path,
        project.join(".codex/config.toml")
    );
    assert!(inventory.items.iter().any(|i| i.name == "fixture-codex"));
    assert_no_secret(&inventory);

    let (location, project) = claude_fixture(root.path());
    write(
        &project.join(".mcp.json"),
        r#"{"mcpServers": {"bad": {"env": {"K": "fixture-env-secret"}}}"#,
    );
    write(
        &project.join(".claude/settings.json"),
        "{\"enabledPlugins\": fixture-header-secret",
    );
    let inventory = read(&AccountId::new("claude-a"), &location, &project);
    let problems: Vec<_> = inventory.problems.iter().map(|p| p.path.clone()).collect();
    assert_eq!(
        problems,
        [
            project.join(".mcp.json"),
            project.join(".claude/settings.json")
        ]
    );
    assert!(inventory.items.iter().any(|i| i.name == "fixture-user"));
    assert_no_secret(&inventory);
}

#[test]
fn missing_homes_list_nothing_and_report_nothing() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("p");
    for location in [
        Location::Claude {
            config_dir: root.path().join("none"),
            global_file: root.path().join("none/.claude.json"),
        },
        Location::Codex {
            home: root.path().join("none"),
            user_home: None,
        },
    ] {
        let inventory = read(&AccountId::new("x"), &location, &project);
        assert_eq!((inventory.items, inventory.problems), (vec![], vec![]));
        assert!(inventory.files.values().all(Option::is_none));
    }
}

#[test]
fn urls_keep_only_scheme_host_and_port() {
    let hidden = "\u{2022}\u{2022}\u{2022}";
    for (url, shown) in [
        (
            "https://u:p@host:8080/a/b?x=1#f",
            "https://host:8080/\u{2026}",
        ),
        ("http://host", "http://host"),
        ("http://host/", "http://host"),
        ("https://host/secret-token/mcp", "https://host/\u{2026}"),
        ("https://host?key=secret", "https://host/\u{2026}"),
        ("https://host:8443/x@secret", "https://host:8443/\u{2026}"),
        ("https://u:pa/ss@host/mcp", hidden),
        ("https://u:pa?ss@host/mcp", hidden),
        ("https://u:pa#ss@host/mcp", hidden),
        ("not a url?secret", hidden),
        ("unix:/run/secret.sock", "unix:\u{2026}"),
    ] {
        assert_eq!(redact_url(url), shown, "{url}");
    }
}

#[test]
fn front_matter_reads_plain_quoted_and_folded_values() {
    let text = "---\nname: \"quoted\"\ndescription: >\n  Folded over\n  two lines.\nother: x\n---\nname: body\n";
    assert_eq!(front_matter(text, "name").as_deref(), Some("quoted"));
    assert_eq!(
        front_matter(text, "description").as_deref(),
        Some("Folded over two lines.")
    );
    assert_eq!(front_matter(text, "missing"), None);
    assert_eq!(front_matter("no front matter", "name"), None);
}

/// Claude keys local servers by the git root, so a leftover entry for the subfolder
/// does not win over it.
#[test]
fn the_git_roots_project_entry_wins_over_a_subfolders() {
    let root = tempfile::tempdir().unwrap();
    let (location, repo) = claude_fixture(root.path());
    fs::create_dir_all(repo.join(".git")).unwrap();
    let sub = repo.join("sub");
    fs::create_dir_all(&sub).unwrap();
    let global = root.path().join("claude-config/.claude.json");
    let mut value: Value = serde_json::from_str(&fs::read_to_string(&global).unwrap()).unwrap();
    value["projects"][sub.to_str().unwrap()] =
        serde_json::json!({"mcpServers": {"stale-local": {"command": "true"}}});
    fs::write(&global, value.to_string()).unwrap();
    let inventory = read(&AccountId::new("claude-a"), &location, &sub);
    let local: Vec<_> = inventory
        .items
        .iter()
        .filter(|i| i.scope == Scope::Local && i.kind() == Kind::McpServer)
        .map(|i| i.name.as_str())
        .collect();
    assert_eq!(local, ["fixture-local"]);
}

/// The nearest `[projects]` entry decides trust, so a subfolder marked untrusted stays
/// untrusted inside a trusted repository.
#[test]
fn codex_trust_comes_from_the_nearest_project_entry() {
    let root = tempfile::tempdir().unwrap();
    let (location, repo) = codex_fixture(root.path(), true);
    fs::create_dir_all(repo.join(".git")).unwrap();
    let sub = repo.join("sub");
    write(
        &sub.join(".codex/config.toml"),
        "[mcp_servers.fixture-sub]\ncommand = \"true\"\n",
    );
    let config = root.path().join("codex-home/config.toml");
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str(&format!(
        "\n[projects.\"{}\"]\ntrust_level = \"untrusted\"\n",
        sub.display()
    ));
    fs::write(&config, text).unwrap();
    let state = |project: &Path, name: &str| {
        read(&AccountId::new("codex-a"), &location, project)
            .items
            .into_iter()
            .find(|i| i.name == name && i.scope == Scope::Project)
            .map(|i| i.state)
    };
    assert_eq!(state(&repo, "fixture-repo"), Some(State::On));
    assert_eq!(state(&sub, "fixture-sub"), Some(State::Untrusted));
    assert_eq!(state(&sub, "fixture-repo"), Some(State::Untrusted));
}

/// With the user's home as the git root, Codex's own `config.toml` and the home's
/// `.agents/skills` stay user scope rather than also being read as the project's.
#[test]
fn a_home_that_is_a_git_root_is_not_read_again_as_the_project() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let codex_home = home.join(".codex");
    let project = home.join("work");
    fs::create_dir_all(home.join(".git")).unwrap();
    fs::create_dir_all(&project).unwrap();
    write(
        &codex_home.join("config.toml"),
        "[mcp_servers.mine]\ncommand = \"true\"\n",
    );
    skill(&home.join(".agents/skills"), "home-skill", "Home skill");
    skill(&codex_home.join("skills"), "codex-skill", "Codex skill");
    let location = Location::Codex {
        home: codex_home,
        user_home: Some(home),
    };
    let inventory = read(&AccountId::new("codex-a"), &location, &project);
    let items: Vec<_> = inventory
        .items
        .iter()
        .map(|i| (i.name.as_str(), i.scope, i.overridden_by))
        .collect();
    assert_eq!(
        items,
        [
            ("mine", Scope::User, None),
            ("codex-skill", Scope::User, None),
            ("home-skill", Scope::User, None),
        ]
    );
}

#[test]
fn codex_plugin_skills_come_from_the_highest_version() {
    let root = tempfile::tempdir().unwrap();
    let (location, project) = codex_fixture(root.path(), false);
    let versions = root
        .path()
        .join("codex-home/plugins/cache/fixture-market/fixture-plugin");
    skill(&versions.join("1.10.0/skills"), "newest-skill", "Newest");
    skill(&versions.join("1.9.0/skills"), "older-skill", "Older");
    let inventory = read(&AccountId::new("codex-a"), &location, &project);
    let plugin_skills: Vec<_> = inventory
        .items
        .iter()
        .filter(|i| {
            matches!(
                &i.detail,
                Detail::Skill {
                    plugin: Some(_),
                    ..
                }
            )
        })
        .map(|i| i.name.as_str())
        .collect();
    assert_eq!(plugin_skills, ["fixture-plugin:newest-skill"]);
}

/// A whole command line pasted into `command` keeps only its first word.
#[test]
fn a_command_line_keeps_only_its_first_word() {
    let expected = Transport::Stdio {
        command: "env".into(),
        args: 3,
    };
    let claude = claude_server(&serde_json::json!({
        "command": "env TOKEN=fixture-arg-secret node",
        "args": ["server.js"]
    }))
    .unwrap();
    assert_eq!(claude.transport, expected);
    let codex: toml::Table =
        toml::from_str("command = \"env TOKEN=fixture-arg-secret node\"\nargs = [\"server.js\"]\n")
            .unwrap();
    assert_eq!(codex_server(&codex).unwrap().transport, expected);
}
