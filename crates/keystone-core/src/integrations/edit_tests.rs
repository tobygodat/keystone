use super::*;
use crate::AccountId;
use crate::integrations::{Inventory, State, read};
use std::cell::RefCell;

// A made-up credential, split so secret scanners don't take it for a real one.
macro_rules! github_token {
    () => {
        concat!("ghp_", "16C7e42F292c6912E7710c838347Ae178B4a")
    };
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// A Codex config as a person writes one: comments, a trailing comment and a
/// sub-table, laid out the way `codex mcp add` would not.
const CODEX: &str = r#"# top comment
model = "o3" # trailing

# servers below
[mcp_servers.alpha]
# alpha comment
command = "node"   # inline
args = ["a.js"]
startup_timeout_sec = 20

[mcp_servers.alpha.env]
TOKEN = "fixture-secret" # kept

[mcp_servers.beta]
url = "https://b.example/mcp"

[plugins."fixture@market"]
enabled = true
"#;

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    project: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_owned();
    let project = root.join("project");
    fs::create_dir_all(project.join(".git")).unwrap();
    Fixture {
        _dir: dir,
        root,
        project,
    }
}

impl Fixture {
    fn codex(&self) -> Location {
        Location::Codex {
            home: self.root.join("codex"),
            user_home: None,
        }
    }

    fn claude(&self) -> Location {
        let config_dir = self.root.join("claude");
        Location::Claude {
            global_file: config_dir.join(".claude.json"),
            config_dir,
        }
    }

    fn read(&self, location: &Location) -> Inventory {
        read(&AccountId::new("a"), location, &self.project)
    }

    fn backups(&self) -> PathBuf {
        self.root.join("backups")
    }
}

fn item<'a>(inventory: &'a Inventory, name: &str) -> &'a Item {
    inventory
        .items
        .iter()
        .find(|item| item.name == name)
        .unwrap_or_else(|| panic!("{name} listed in {:?}", inventory.items))
}

/// Applies `change` against what `inventory` showed, recording each CLI call.
fn change(
    fixture: &Fixture,
    location: &Location,
    default: bool,
    inventory: &Inventory,
    change: Change,
    cli: Result<(), String>,
) -> (Result<(), Failure>, Vec<Cli>) {
    let calls = RefCell::new(Vec::new());
    let plan = plan(location, default, &fixture.project, change).expect("planned");
    let result = apply(plan, &inventory.files, &fixture.backups(), &mut |call| {
        calls.borrow_mut().push(call.clone());
        cli.clone()
    });
    (result, calls.into_inner())
}

/// The lines of `before` and `after` that differ, as `-old` and `+new`.
fn diff(before: &str, after: &str) -> Vec<String> {
    let (a, b): (Vec<_>, Vec<_>) = (before.lines().collect(), after.lines().collect());
    let start = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let end = a[start..]
        .iter()
        .rev()
        .zip(b[start..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let removed = a[start..a.len() - end].iter().map(|l| format!("-{l}"));
    let added = b[start..b.len() - end].iter().map(|l| format!("+{l}"));
    removed.chain(added).collect()
}

fn command(command: &str, args: &[&str], env: &[(&str, Option<&str>)]) -> Server {
    Server {
        target: Target::Command {
            command: command.into(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
        },
        env: env
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.map(str::to_owned)))
            .collect(),
    }
}

#[test]
fn turning_a_codex_server_off_touches_only_that_server_and_keeps_comments() {
    let fixture = fixture();
    let location = fixture.codex();
    let config = fixture.root.join("codex/config.toml");
    write(&config, CODEX);
    let shown = fixture.read(&location);
    let alpha = item(&shown, "alpha").clone();
    let (result, calls) = change(
        &fixture,
        &location,
        true,
        &shown,
        Change::Switch {
            item: alpha.clone(),
            on: false,
        },
        Ok(()),
    );
    assert_eq!(result, Ok(()));
    assert!(calls.is_empty(), "no CLI turns a Codex server off");
    let after = fs::read_to_string(&config).unwrap();
    assert_eq!(
        diff(CODEX, &after),
        ["+enabled = false"],
        "the new line sits in alpha's own table:\n{after}"
    );
    assert!(after.contains("startup_timeout_sec = 20\nenabled = false\n\n[mcp_servers.alpha.env]"));
    assert_eq!(item(&fixture.read(&location), "alpha").state, State::Off);

    let shown = fixture.read(&location);
    let alpha = item(&shown, "alpha").clone();
    let on = Change::Switch {
        item: alpha,
        on: true,
    };
    assert_eq!(
        change(&fixture, &location, true, &shown, on, Ok(())).0,
        Ok(())
    );
    assert_eq!(fs::read_to_string(&config).unwrap(), CODEX);
    let backups: Vec<_> = walk(&fixture.backups());
    assert_eq!(backups.len(), 2, "each write keeps the file it replaced");
    assert!(backups.iter().all(|b| b.ends_with("codex/config.toml")));
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(root).into_iter().flatten().flatten() {
        if entry.path().is_dir() {
            found.extend(walk(&entry.path()));
        } else {
            found.push(entry.path());
        }
    }
    found.sort();
    found
}

#[test]
fn a_file_edited_after_it_was_shown_is_left_alone() {
    let fixture = fixture();
    let location = fixture.codex();
    let config = fixture.root.join("codex/config.toml");
    write(&config, CODEX);
    let shown = fixture.read(&location);
    let edited = format!("{CODEX}# edited elsewhere\n");
    write(&config, &edited);
    for change_to in [
        Change::Switch {
            item: item(&shown, "alpha").clone(),
            on: false,
        },
        Change::Remove {
            item: item(&shown, "beta").clone(),
        },
    ] {
        let (result, calls) = change(&fixture, &location, true, &shown, change_to, Ok(()));
        assert_eq!(result, Err(Failure::Conflict(config.clone())));
        assert!(calls.is_empty(), "nothing runs: {calls:?}");
    }
    assert_eq!(fs::read_to_string(&config).unwrap(), edited);
    assert!(!fixture.backups().exists());
}

#[test]
fn claude_rewriting_its_own_counters_is_not_an_outside_edit() {
    let path = Path::new("/x/.claude.json");
    let a = r#"{"numStartups": 1, "mcpServers": {"s": {"command": "a"}}, "projects": {"/p": {"lastCost": 1, "disabledMcpServers": []}}}"#;
    let b = r#"{"numStartups": 2, "mcpServers": {"s": {"command": "a"}}, "projects": {"/p": {"lastCost": 3, "disabledMcpServers": []}}}"#;
    let c = r#"{"numStartups": 2, "mcpServers": {"s": {"command": "b"}}, "projects": {"/p": {"lastCost": 3, "disabledMcpServers": []}}}"#;
    let d = r#"{"numStartups": 2, "mcpServers": {"s": {"command": "a"}}, "projects": {"/p": {"lastCost": 3, "disabledMcpServers": ["s"]}}}"#;
    let project = Path::new("/p");
    let print = |text| crate::integrations::fingerprint(path, Some(text), project);
    assert_eq!(print(a), print(b));
    assert_ne!(print(a), print(c));
    assert_ne!(print(a), print(d));
    let elsewhere = r#"{"numStartups": 2, "mcpServers": {"s": {"command": "a"}}, "projects": {"/p": {"lastCost": 3, "disabledMcpServers": []}, "/other": {"mcpServers": {"x": {}}}}}"#;
    assert_eq!(
        print(a),
        print(elsewhere),
        "a session in another folder is not an outside edit"
    );
    let other = Path::new("/x/.mcp.json");
    assert_ne!(
        crate::integrations::fingerprint(other, Some(a), project),
        crate::integrations::fingerprint(other, Some(b), project)
    );
}

#[test]
fn adding_a_claude_server_runs_its_cli_at_the_chosen_scope() {
    let fixture = fixture();
    let location = fixture.claude();
    let shown = fixture.read(&location);
    let add = |scope| Change::AddServer {
        scope,
        name: "added".into(),
        server: command("npx", &["srv", "--flag"], &[("API_KEY", Some("k"))]),
    };
    for (scope, word) in [
        (Scope::User, "user"),
        (Scope::Local, "local"),
        (Scope::Project, "project"),
    ] {
        let (result, calls) = change(&fixture, &location, true, &shown, add(scope), Ok(()));
        assert_eq!(result, Ok(()));
        assert_eq!(
            calls,
            [Cli {
                args: [
                    "mcp",
                    "add-json",
                    "-s",
                    word,
                    "added",
                    r#"{"args":["srv","--flag"],"command":"npx","env":{"API_KEY":"k"},"type":"stdio"}"#
                ]
                .map(String::from)
                .to_vec(),
                cwd: fixture.project.clone(),
            }]
        );
    }
    let url = Change::AddServer {
        scope: Scope::User,
        name: "remote".into(),
        server: Server {
            target: Target::Url("https://m.example/mcp".into()),
            env: Vec::new(),
        },
    };
    let (_, calls) = change(&fixture, &location, true, &shown, url, Ok(()));
    assert_eq!(
        calls[0].args.last().unwrap(),
        r#"{"type":"http","url":"https://m.example/mcp"}"#
    );
    let failed = change(
        &fixture,
        &location,
        true,
        &shown,
        add(Scope::User),
        Err("exists".into()),
    );
    assert_eq!(failed.0, Err(Failure::Failed("exists".into())));
}

#[test]
fn a_claude_cli_that_echoes_a_credential_in_the_command_shows_none_of_it() {
    let fixture = fixture();
    let location = fixture.claude();
    let shown = fixture.read(&location);
    let token = github_token!();
    let add = Change::AddDefinition {
        scope: Scope::User,
        name: "added".into(),
        definition: serde_json::json!({
            "type": "stdio",
            "command": format!("node server.js --token {token} --password \"correct horse battery staple\"")
        }),
    };
    let echo = format!("refused --token {token} --password correct horse battery staple");
    let (result, _) = change(&fixture, &location, true, &shown, add, Err(echo));
    let Err(Failure::Failed(error)) = result else {
        panic!("{result:?}")
    };
    assert!(!error.contains(token), "{error}");
    assert!(!error.contains("correct"), "{error}");
}

#[test]
fn a_claude_cli_that_echoes_only_a_value_or_a_credential_shows_none_of_it() {
    let fixture = fixture();
    let location = fixture.claude();
    let shown = fixture.read(&location);
    let token = github_token!();
    let header = "Authorization: Bearer opaquecredential123";
    let url = "https://mcp.example.com/s/aB3dE5fG7hJ9kL1mN3pQ5/mcp";
    let cases = [
        (
            serde_json::json!({"command": "node", "args": ["server.js", format!("--token={token}")]}),
            token,
        ),
        (
            serde_json::json!({"command": "env API_KEY=sk-proj-abc123def456ghi789 node server.js"}),
            "sk-proj-abc123def456ghi789",
        ),
        (
            serde_json::json!({"command": "npx", "args": ["mcp-remote", "https://example.com/mcp", "--header", header]}),
            "opaquecredential123",
        ),
        (
            serde_json::json!({"command": format!("npx mcp-remote https://example.com/mcp --header \"{header}\"")}),
            "opaquecredential123",
        ),
        (
            serde_json::json!({"command": "node", "args": ["server.js", "--password=correct horse battery staple"]}),
            "correct horse battery staple",
        ),
        (
            serde_json::json!({"command": "node server.js --password correct\\ horse\\ battery\\ staple"}),
            "correct horse battery staple",
        ),
        (
            serde_json::json!({"command": "npx", "args": ["mcp-remote", url]}),
            "aB3dE5fG7hJ9kL1mN3pQ5",
        ),
        (
            serde_json::json!({"command": format!("npx mcp-remote {url}")}),
            "aB3dE5fG7hJ9kL1mN3pQ5",
        ),
        (
            serde_json::json!({"type": "http", "url": url}),
            "aB3dE5fG7hJ9kL1mN3pQ5",
        ),
        (
            serde_json::json!({"type": "http", "url": "https://alice:hunter2hunter2@mcp.example.com/mcp?region=us#access_token=opaqueCredential123"}),
            "hunter2hunter2 opaqueCredential123",
        ),
    ];
    for (mut definition, secret) in cases {
        if definition["type"].is_null() {
            definition["type"] = "stdio".into();
        }
        let add = Change::AddDefinition {
            scope: Scope::User,
            name: "added".into(),
            definition,
        };
        let echo = format!("refused {secret}");
        let (result, _) = change(&fixture, &location, true, &shown, add, Err(echo));
        let Err(Failure::Failed(error)) = result else {
            panic!("{result:?}")
        };
        for word in secret.split_whitespace() {
            assert!(!error.contains(word), "{error}");
        }
    }
}

fn claude_fixture(fixture: &Fixture) -> PathBuf {
    let global = fixture.root.join("claude/.claude.json");
    write(
        &global,
        &format!(
            "{{\n  \"numStartups\": 4,\n  \"mcpServers\": {{\n    \"user-server\": {{\"type\": \"stdio\", \"command\": \"python3\", \"args\": [\"s.py\"], \"env\": {{\"TOKEN\": \"fixture-secret\", \"DROP\": \"x\"}}, \"timeout\": 5}}\n  }},\n  \"projects\": {{\n    \"{}\": {{\"allowedTools\": [], \"mcpServers\": {{}}}}\n  }},\n  \"zLast\": true\n}}\n",
            fixture.project.display()
        ),
    );
    write(
        &fixture.project.join(".mcp.json"),
        r#"{"mcpServers": {"shared": {"command": "true"}}}"#,
    );
    global
}

#[test]
fn the_default_claude_accounts_own_file_is_never_edited() {
    let fixture = fixture();
    let location = fixture.claude();
    let global = claude_fixture(&fixture);
    let shown = fixture.read(&location);
    let off = Change::Switch {
        item: item(&shown, "user-server").clone(),
        on: false,
    };
    let refused = plan(&location, true, &fixture.project, off.clone()).err();
    assert_eq!(refused.as_deref(), Some(ONLY_CLAUDE));

    assert_eq!(
        change(&fixture, &location, false, &shown, off, Ok(())).0,
        Ok(())
    );
    let after = fs::read_to_string(&global).unwrap();
    let value: Value = serde_json::from_str(&after).unwrap();
    let key = fixture.project.to_str().unwrap();
    assert_eq!(
        value["projects"][key]["disabledMcpServers"],
        serde_json::json!(["user-server"])
    );
    let at = |key: &str| after.find(&format!("\n  \"{key}\"")).unwrap();
    assert!(
        at("numStartups") < at("mcpServers") && at("projects") < at("zLast"),
        "{after}"
    );
    assert!(
        after.contains("\"disabledMcpServers\": [\n        \"user-server\"\n      ]"),
        "{after}"
    );
    assert_eq!(
        item(&fixture.read(&location), "user-server").state,
        State::Off
    );
}

#[test]
fn a_claude_project_server_turns_off_in_the_projects_local_settings() {
    let fixture = fixture();
    let location = fixture.claude();
    let global = claude_fixture(&fixture);
    let before = fs::read_to_string(&global).unwrap();
    let local = fixture.project.join(".claude/settings.local.json");
    write(&local, "{\n  \"permissions\": {\"allow\": []}\n}\n");
    let shown = fixture.read(&location);
    let off = Change::Switch {
        item: item(&shown, "shared").clone(),
        on: false,
    };
    assert_eq!(
        change(&fixture, &location, true, &shown, off, Ok(())).0,
        Ok(())
    );
    assert_eq!(
        fs::read_to_string(&local).unwrap(),
        "{\n  \"permissions\": {\n    \"allow\": []\n  },\n  \"disabledMcpjsonServers\": [\n    \"shared\"\n  ]\n}\n"
    );
    assert_eq!(fs::read_to_string(&global).unwrap(), before);
    let shown = fixture.read(&location);
    assert_eq!(item(&shown, "shared").state, State::Off);
    let on = Change::Switch {
        item: item(&shown, "shared").clone(),
        on: true,
    };
    assert_eq!(
        change(&fixture, &location, true, &shown, on, Ok(())).0,
        Ok(())
    );
    assert_eq!(item(&fixture.read(&location), "shared").state, State::On);
}

#[test]
fn editing_a_claude_server_keeps_hidden_values_and_undoes_a_failed_add() {
    let fixture = fixture();
    let location = fixture.claude();
    claude_fixture(&fixture);
    let shown = fixture.read(&location);
    let user = item(&shown, "user-server").clone();
    let form = server(&location, &fixture.project, &user).unwrap();
    assert_eq!(
        form,
        command("python3", &["s.py"], &[("DROP", None), ("TOKEN", None)])
    );
    let edit = Change::Edit {
        item: user,
        server: command("python3", &["t.py"], &[("TOKEN", None), ("NEW", Some("n"))]),
    };
    let calls = RefCell::new(Vec::new());
    let edit_plan = plan(&location, true, &fixture.project, edit).unwrap();
    let result = apply(edit_plan, &shown.files, &fixture.backups(), &mut |call| {
        calls.borrow_mut().push(call.args.clone());
        match call.args[1].as_str() {
            "add-json" if calls.borrow().len() == 2 => Err("refused".into()),
            _ => Ok(()),
        }
    });
    assert_eq!(result, Err(Failure::Failed("refused".into())));
    let calls = calls.into_inner();
    let strings = |args: &[&str]| args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        calls,
        [
            strings(&["mcp", "remove", "-s", "user", "user-server"]),
            strings(&[
                "mcp",
                "add-json",
                "-s",
                "user",
                "user-server",
                r#"{"args":["t.py"],"command":"python3","env":{"NEW":"n","TOKEN":"fixture-secret"},"timeout":5,"type":"stdio"}"#
            ]),
            strings(&[
                "mcp",
                "add-json",
                "-s",
                "user",
                "user-server",
                r#"{"args":["s.py"],"command":"python3","env":{"DROP":"x","TOKEN":"fixture-secret"},"timeout":5,"type":"stdio"}"#
            ]),
        ]
    );
    let unknown = Change::Edit {
        item: item(&shown, "user-server").clone(),
        server: command("python3", &[], &[("MISSING", None)]),
    };
    assert_eq!(
        plan(&location, true, &fixture.project, unknown)
            .err()
            .as_deref(),
        Some("Give MISSING a value.")
    );
}

#[test]
fn codex_servers_are_added_and_removed_by_its_cli_at_user_scope_and_edited_in_place() {
    let fixture = fixture();
    let location = fixture.codex();
    let config = fixture.root.join("codex/config.toml");
    write(&config, CODEX);
    let shown = fixture.read(&location);
    let add = Change::AddServer {
        scope: Scope::User,
        name: "gamma".into(),
        server: command("python3", &["-m", "srv"], &[("K", Some("v"))]),
    };
    let (_, calls) = change(&fixture, &location, true, &shown, add, Ok(()));
    assert_eq!(
        calls[0].args,
        [
            "mcp", "add", "gamma", "--env", "K=v", "--", "python3", "-m", "srv"
        ]
    );
    let remove = Change::Remove {
        item: item(&shown, "beta").clone(),
    };
    let (_, calls) = change(&fixture, &location, true, &shown, remove, Ok(()));
    assert_eq!(calls[0].args, ["mcp", "remove", "beta"]);

    let edit = Change::Edit {
        item: item(&shown, "alpha").clone(),
        server: command("deno", &["run", "x.ts"], &[("TOKEN", None)]),
    };
    assert_eq!(
        change(&fixture, &location, true, &shown, edit, Ok(())).0,
        Ok(())
    );
    let after = fs::read_to_string(&config).unwrap();
    assert_eq!(
        diff(CODEX, &after),
        [
            "-command = \"node\"   # inline",
            "-args = [\"a.js\"]",
            "+command = \"deno\"   # inline",
            "+args = [\"run\", \"x.ts\"]"
        ],
        "{after}"
    );
}

#[test]
fn a_codex_project_server_is_added_to_the_projects_own_config() {
    let fixture = fixture();
    let location = fixture.codex();
    let project_config = fixture.project.join(".codex/config.toml");
    write(&project_config, "# project\nmodel = \"o3\"\n");
    let shown = fixture.read(&location);
    let add = |name: &str| Change::AddServer {
        scope: Scope::Project,
        name: name.into(),
        server: command("node", &["p.js"], &[("K", Some("v"))]),
    };
    let (result, calls) = change(&fixture, &location, true, &shown, add("proj"), Ok(()));
    assert_eq!(result, Ok(()));
    assert!(calls.is_empty());
    assert_eq!(
        fs::read_to_string(&project_config).unwrap(),
        "# project\nmodel = \"o3\"\n\n[mcp_servers.proj]\ncommand = \"node\"\nargs = [\"p.js\"]\n\n[mcp_servers.proj.env]\nK = \"v\"\n"
    );
    let shown = fixture.read(&location);
    assert_eq!(
        change(&fixture, &location, true, &shown, add("proj"), Ok(())).0,
        Err(Failure::Failed(
            "A server named proj is already there.".into()
        ))
    );
    let remove = Change::Remove {
        item: item(&shown, "proj").clone(),
    };
    assert_eq!(
        change(&fixture, &location, true, &shown, remove, Ok(())).0,
        Ok(())
    );
    assert_eq!(
        fs::read_to_string(&project_config).unwrap(),
        "# project\nmodel = \"o3\"\n"
    );
}

#[test]
fn codex_plugins_switch_in_config_toml() {
    let fixture = fixture();
    let location = fixture.codex();
    let config = fixture.root.join("codex/config.toml");
    write(&config, CODEX);
    let shown = fixture.read(&location);
    let off = Change::Switch {
        item: item(&shown, "fixture@market").clone(),
        on: false,
    };
    assert_eq!(
        change(&fixture, &location, true, &shown, off, Ok(())).0,
        Ok(())
    );
    assert_eq!(
        diff(CODEX, &fs::read_to_string(&config).unwrap()),
        ["-enabled = true", "+enabled = false"]
    );
}

#[test]
fn claude_plugins_switch_through_its_cli() {
    let fixture = fixture();
    let location = fixture.claude();
    write(
        &fixture.root.join("claude/settings.json"),
        r#"{"enabledPlugins": {"p@m": true}}"#,
    );
    let shown = fixture.read(&location);
    let off = Change::Switch {
        item: item(&shown, "p@m").clone(),
        on: false,
    };
    let (_, calls) = change(&fixture, &location, true, &shown, off, Ok(()));
    assert_eq!(calls[0].args, ["plugin", "disable", "-s", "user", "p@m"]);
}

#[test]
fn skill_folders_are_copied_in_and_removed_to_the_backups() {
    let fixture = fixture();
    let location = fixture.codex();
    let source = fixture.root.join("elsewhere/my-skill");
    write(
        &source.join("SKILL.md"),
        "---\nname: my-skill\ndescription: Mine\n---\n",
    );
    write(&source.join("scripts/run.sh"), "echo hi\n");
    let shown = fixture.read(&location);
    let add = Change::AddSkill {
        scope: Scope::Project,
        folder: source.clone(),
        content: None,
    };
    assert_eq!(
        change(&fixture, &location, true, &shown, add.clone(), Ok(())).0,
        Ok(())
    );
    let copied = fixture.project.join(".agents/skills/my-skill");
    assert_eq!(
        fs::read_to_string(copied.join("scripts/run.sh")).unwrap(),
        "echo hi\n"
    );
    assert_eq!(
        change(&fixture, &location, true, &shown, add, Ok(())).0,
        Err(Failure::Failed(
            "A skill named my-skill is already there.".into()
        ))
    );
    let shown = fixture.read(&location);
    let remove = Change::Remove {
        item: item(&shown, "my-skill").clone(),
    };
    assert_eq!(
        change(&fixture, &location, true, &shown, remove, Ok(())).0,
        Ok(())
    );
    assert!(!copied.exists());
    assert!(source.join("SKILL.md").is_file(), "the original stays");
    let kept = walk(&fixture.backups());
    assert!(
        kept.iter()
            .any(|path| path.ends_with("my-skill/scripts/run.sh")),
        "{kept:?}"
    );

    let empty = fixture.root.join("empty");
    fs::create_dir_all(&empty).unwrap();
    let none = Change::AddSkill {
        scope: Scope::User,
        folder: empty,
        content: None,
    };
    assert_eq!(
        plan(&location, true, &fixture.project, none)
            .err()
            .as_deref(),
        Some("That folder has no SKILL.md.")
    );
}

#[test]
fn a_skill_copy_that_no_longer_has_the_previewed_content_lands_nothing() {
    let fixture = fixture();
    let location = fixture.codex();
    let source = fixture.root.join("elsewhere/my-skill");
    write(&source.join("SKILL.md"), "---\nname: my-skill\n---\n");
    let previewed = fingerprint_now(&source, &fixture.project).unwrap();
    write(
        &source.join("SKILL.md"),
        "---\nname: my-skill\n---\nEdited.\n",
    );
    let shown = fixture.read(&location);
    let add = |content| Change::AddSkill {
        scope: Scope::Project,
        folder: source.clone(),
        content,
    };
    let root = fixture.project.join(".agents/skills");
    assert_eq!(
        change(&fixture, &location, true, &shown, add(previewed), Ok(())).0,
        Err(Failure::Conflict(source.clone()))
    );
    assert_eq!(
        fs::read_dir(&root).map_or(0, Iterator::count),
        0,
        "nothing lands, staged or not"
    );
    let now = fingerprint_now(&source, &fixture.project).unwrap();
    assert_eq!(
        change(&fixture, &location, true, &shown, add(now), Ok(())).0,
        Ok(())
    );
    assert_eq!(
        fs::read_to_string(root.join("my-skill/SKILL.md")).unwrap(),
        "---\nname: my-skill\n---\nEdited.\n"
    );
}

#[cfg(unix)]
#[test]
fn a_replaced_file_keeps_its_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = fixture();
    let location = fixture.codex();
    let config = fixture.root.join("codex/config.toml");
    write(&config, CODEX);
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    let shown = fixture.read(&location);
    let off = Change::Switch {
        item: item(&shown, "beta").clone(),
        on: false,
    };
    assert_eq!(
        change(&fixture, &location, true, &shown, off, Ok(())).0,
        Ok(())
    );
    let mode = fs::metadata(&config).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let leftovers: Vec<_> = fs::read_dir(config.parent().unwrap())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("keystone"))
        .collect();
    assert!(leftovers.is_empty());
}

#[test]
fn adding_a_codex_server_under_a_name_in_use_is_refused_before_the_cli_runs() {
    let fixture = fixture();
    let location = fixture.codex();
    write(&fixture.root.join("codex/config.toml"), CODEX);
    let add = Change::AddServer {
        scope: Scope::User,
        name: "alpha".into(),
        server: command("deno", &[], &[]),
    };
    assert_eq!(
        plan(&location, true, &fixture.project, add)
            .err()
            .as_deref(),
        Some("A server named alpha is already there.")
    );
}

#[test]
fn an_added_claude_account_turns_a_user_server_off_in_a_project_claude_has_not_seen() {
    let fixture = fixture();
    let location = fixture.claude();
    let global = fixture.root.join("claude/.claude.json");
    write(
        &global,
        "{\n  \"mcpServers\": {\"s\": {\"command\": \"true\"}}\n}\n",
    );
    let shown = fixture.read(&location);
    let off = Change::Switch {
        item: item(&shown, "s").clone(),
        on: false,
    };
    assert_eq!(
        change(&fixture, &location, false, &shown, off, Ok(())).0,
        Ok(())
    );
    let value: Value = serde_json::from_str(&fs::read_to_string(&global).unwrap()).unwrap();
    let key = fixture.project.to_str().unwrap();
    assert_eq!(
        value["projects"][key]["disabledMcpServers"],
        serde_json::json!(["s"])
    );
    assert_eq!(item(&fixture.read(&location), "s").state, State::Off);
}

#[test]
fn a_project_server_turned_off_with_slash_mcp_turns_back_on() {
    let fixture = fixture();
    let location = fixture.claude();
    let global = fixture.root.join("claude/.claude.json");
    let key = fixture.project.to_str().unwrap().to_owned();
    write(
        &global,
        &serde_json::json!({"projects": {key.clone(): {"disabledMcpServers": ["shared"]}}})
            .to_string(),
    );
    write(
        &fixture.project.join(".mcp.json"),
        r#"{"mcpServers": {"shared": {"command": "true"}}}"#,
    );
    let shown = fixture.read(&location);
    assert_eq!(item(&shown, "shared").state, State::Off);
    let on = || Change::Switch {
        item: item(&shown, "shared").clone(),
        on: true,
    };
    assert_eq!(
        plan(&location, true, &fixture.project, on())
            .err()
            .as_deref(),
        Some(ONLY_CLAUDE),
        "only Claude edits the default account's own file"
    );
    assert_eq!(
        change(&fixture, &location, false, &shown, on(), Ok(())).0,
        Ok(())
    );
    assert_eq!(item(&fixture.read(&location), "shared").state, State::On);
}

#[test]
fn removing_a_skill_whose_folder_changed_since_it_was_shown_is_a_conflict() {
    let fixture = fixture();
    let location = fixture.codex();
    let folder = fixture.root.join("codex/skills/mine");
    write(&folder.join("SKILL.md"), "---\nname: mine\n---\n");
    write(&folder.join("run.sh"), "one\n");
    let shown = fixture.read(&location);
    write(&folder.join("run.sh"), "two\n");
    let remove = Change::Remove {
        item: item(&shown, "mine").clone(),
    };
    let (result, _) = change(&fixture, &location, true, &shown, remove, Ok(()));
    assert_eq!(result, Err(Failure::Conflict(folder.clone())));
    assert_eq!(fs::read_to_string(folder.join("run.sh")).unwrap(), "two\n");
}

#[test]
fn a_save_from_elsewhere_after_the_first_check_is_still_not_overwritten() {
    let fixture = fixture();
    let location = fixture.claude();
    let global = claude_fixture(&fixture);
    let local = fixture.project.join(".claude/settings.local.json");
    let shown = fixture.read(&location);
    let off = Change::Switch {
        item: item(&shown, "shared").clone(),
        on: false,
    };
    let mut steps = plan(&location, true, &fixture.project, off).unwrap();
    // A CLI step first, during which an editor saves the file the edit then writes.
    steps.steps.insert(
        0,
        Step::Cli(Cli {
            args: vec![],
            cwd: fixture.project.clone(),
        }),
    );
    let result = apply(steps, &shown.files, &fixture.backups(), &mut |_| {
        write(&local, "{\"saved\": \"elsewhere\"}\n");
        Ok(())
    });
    assert_eq!(
        result,
        Err(Failure::Failed(format!(
            "{} changed while saving.",
            local.display()
        )))
    );
    assert_eq!(
        fs::read_to_string(&local).unwrap(),
        "{\"saved\": \"elsewhere\"}\n"
    );
    let _ = global;
}

#[cfg(unix)]
#[test]
fn backups_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = fixture();
    let location = fixture.codex();
    write(&fixture.root.join("codex/config.toml"), CODEX);
    let shown = fixture.read(&location);
    let off = Change::Switch {
        item: item(&shown, "beta").clone(),
        on: false,
    };
    assert_eq!(
        change(&fixture, &location, true, &shown, off, Ok(())).0,
        Ok(())
    );
    let copy = walk(&fixture.backups()).pop().unwrap();
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(copy.parent().unwrap()), 0o700);
    assert_eq!(
        mode(
            fs::read_dir(fixture.backups())
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path()
                .as_path()
        ),
        0o700
    );
}

#[test]
fn a_failed_restore_says_so_and_where_the_old_file_is() {
    let fixture = fixture();
    let location = fixture.claude();
    claude_fixture(&fixture);
    let shown = fixture.read(&location);
    let edit = Change::Edit {
        item: item(&shown, "user-server").clone(),
        server: command("python3", &["t.py"], &[]),
    };
    let edit_plan = plan(&location, true, &fixture.project, edit).unwrap();
    let result = apply(
        edit_plan,
        &shown.files,
        &fixture.backups(),
        &mut |call| match call.args[1].as_str() {
            "remove" => Ok(()),
            _ => Err(format!("refused {}", call.args.last().unwrap())),
        },
    );
    let Err(Failure::Failed(error)) = result else {
        panic!("{result:?}")
    };
    assert!(error.contains("Putting it back failed too"), "{error}");
    assert!(
        error.contains(fixture.backups().to_str().unwrap()),
        "{error}"
    );
    assert!(
        !error.contains("fixture-secret"),
        "a CLI's echo is scrubbed: {error}"
    );
    assert!(!error.contains("t.py"), "{error}");
}

#[cfg(unix)]
#[test]
fn a_symlinked_config_stays_a_link() {
    let fixture = fixture();
    let location = fixture.codex();
    let real = fixture.root.join("dotfiles/config.toml");
    write(&real, CODEX);
    fs::create_dir_all(fixture.root.join("codex")).unwrap();
    std::os::unix::fs::symlink(&real, fixture.root.join("codex/config.toml")).unwrap();
    let shown = fixture.read(&location);
    let off = Change::Switch {
        item: item(&shown, "beta").clone(),
        on: false,
    };
    assert_eq!(
        change(&fixture, &location, true, &shown, off, Ok(())).0,
        Ok(())
    );
    let link = fixture.root.join("codex/config.toml");
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        fs::read_to_string(&real)
            .unwrap()
            .contains("enabled = false")
    );
}

#[cfg(unix)]
#[test]
fn a_change_that_fails_partway_puts_back_what_it_already_wrote() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = fixture();
    let location = fixture.claude();
    let home = fixture.root.join("claude");
    let key = fixture.project.to_str().unwrap().to_owned();
    write(
        &home.join(".claude.json"),
        &serde_json::json!({"projects": {key: {"disabledMcpServers": ["shared"]}}}).to_string(),
    );
    write(
        &fixture.project.join(".mcp.json"),
        r#"{"mcpServers": {"shared": {"command": "true"}}}"#,
    );
    let local = fixture.project.join(".claude/settings.local.json");
    let before = r#"{"disabledMcpjsonServers": ["shared"], "kept": 1}"#;
    write(&local, before);
    let shown = fixture.read(&location);
    let on = Change::Switch {
        item: item(&shown, "shared").clone(),
        on: true,
    };
    fs::set_permissions(&home, fs::Permissions::from_mode(0o500)).unwrap();
    let result = change(&fixture, &location, false, &shown, on, Ok(())).0;
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(result, Err(Failure::Failed(_))), "{result:?}");
    assert_eq!(fs::read_to_string(&local).unwrap(), before);
}

#[cfg(unix)]
#[test]
fn a_skill_holding_a_link_to_a_folder_can_be_removed() {
    let fixture = fixture();
    let location = fixture.codex();
    let folder = fixture.root.join("codex/skills/mine");
    write(&folder.join("SKILL.md"), "---\nname: mine\n---\n");
    fs::create_dir_all(fixture.root.join("shared")).unwrap();
    std::os::unix::fs::symlink(fixture.root.join("shared"), folder.join("shared")).unwrap();
    let shown = fixture.read(&location);
    let remove = Change::Remove {
        item: item(&shown, "mine").clone(),
    };
    assert_eq!(
        change(&fixture, &location, false, &shown, remove, Ok(())).0,
        Ok(())
    );
    assert!(!folder.exists());
    assert!(
        fixture.root.join("shared").is_dir(),
        "the linked folder stays"
    );
}
