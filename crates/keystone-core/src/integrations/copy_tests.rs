use super::*;
use crate::integrations::{Inventory, read};
use edit::{apply, plan};
use serde_json::json;

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    project: PathBuf,
    claude: Location,
    codex: Location,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_owned();
    let project = root.join("project");
    fs::create_dir_all(project.join(".git")).unwrap();
    let config_dir = root.join("claude");
    Fixture {
        claude: Location::Claude {
            global_file: config_dir.join(".claude.json"),
            config_dir,
        },
        codex: Location::Codex {
            home: root.join("codex"),
            user_home: None,
        },
        _dir: dir,
        root,
        project,
    }
}

impl Fixture {
    fn read(&self, location: &Location) -> Inventory {
        read(&AccountId::new("a"), location, &self.project)
    }

    fn item(&self, location: &Location, name: &str) -> Item {
        self.read(location)
            .items
            .into_iter()
            .find(|item| item.name == name)
            .unwrap_or_else(|| panic!("{name} is listed"))
    }

    fn to<'a>(&'a self, location: &'a Location) -> (&'a Location, &'a AccountId, Scope) {
        static ACCOUNT: std::sync::LazyLock<AccountId> =
            std::sync::LazyLock::new(|| AccountId::new("a"));
        (location, &ACCOUNT, Scope::User)
    }

    /// Makes the copy `preview` describes to `location`, which no CLI may take.
    fn copy(&self, location: &Location, preview: Preview) {
        let shown = self.read(location).files;
        let steps = plan(location, false, &self.project, preview.change).unwrap();
        apply(steps, &shown, &self.root.join("backups"), &mut |call| {
            panic!("no CLI runs: {call:?}")
        })
        .unwrap();
    }
}

fn claude_server(fixture: &Fixture, server: Value) {
    let global = json!({"mcpServers": {"github": server}});
    write(
        &fixture.root.join("claude/.claude.json"),
        &serde_json::to_string_pretty(&global).unwrap(),
    );
}

fn github() -> Value {
    json!({
        "type": "stdio",
        "command": "npx",
        "args": ["-y", "server-github", "--api-key", "sk-arg-secret", "--port=9"],
        "env": {"GITHUB_TOKEN": "ghp-env-secret", "LOG_LEVEL": "debug", "HOME_DIR": "${HOME_DIR}"}
    })
}

#[test]
fn a_claude_server_copies_to_codex_as_previewed_with_only_the_secrets_ticked() {
    let fixture = fixture();
    claude_server(&fixture, github());
    let config = fixture.root.join("codex/config.toml");
    let before = "# mine\nmodel = \"o3\"\n";
    write(&config, before);
    let item = fixture.item(&fixture.claude, "github");

    let none = preview(
        &item,
        &fixture.claude,
        fixture.to(&fixture.codex),
        &fixture.project,
        &[],
    )
    .unwrap();
    assert_eq!(none.to, config);
    assert_eq!(none.secrets, ["GITHUB_TOKEN", "--api-key"]);
    assert_eq!(none.pair.left_out, ["GITHUB_TOKEN", "--api-key"]);
    assert_eq!(
        none.text,
        "[mcp_servers.github]\nargs = [\"-y\", \"server-github\", \"--port=9\"]\ncommand = \"npx\"\nenv_vars = [\"HOME_DIR\"]\n\n[mcp_servers.github.env]\nLOG_LEVEL = \"debug\"\n",
        "a secret left out is not written at all"
    );

    let include = ["GITHUB_TOKEN".to_owned()];
    let one = preview(
        &item,
        &fixture.claude,
        fixture.to(&fixture.codex),
        &fixture.project,
        &include,
    )
    .unwrap();
    assert_eq!(one.pair.left_out, ["--api-key"]);
    assert!(!one.text.contains("ghp-env-secret"), "{}", one.text);
    assert!(
        one.text.contains(&format!("GITHUB_TOKEN = \"{MASK}\"")),
        "{}",
        one.text
    );
    let written = one.text.replace(MASK, "ghp-env-secret");
    fixture.copy(&fixture.codex, one);
    assert_eq!(
        fs::read_to_string(&config).unwrap(),
        format!("{before}\n{written}"),
        "the file changes exactly as previewed"
    );
}

#[test]
fn a_copy_compares_both_ends_and_names_what_it_still_needs() {
    let fixture = fixture();
    claude_server(&fixture, github());
    let item = fixture.item(&fixture.claude, "github");
    let include = ["--api-key".to_owned()];
    let made = preview(
        &item,
        &fixture.claude,
        fixture.to(&fixture.codex),
        &fixture.project,
        &include,
    )
    .unwrap();
    let pair = made.pair.clone();
    fixture.copy(&fixture.codex, made);
    let drift = || drift(&pair, &fixture.claude, &fixture.codex, &fixture.project);
    assert_eq!(drift(), Some(Drift::Needs(vec!["GITHUB_TOKEN".into()])));

    let config = fixture.root.join("codex/config.toml");
    let text = fs::read_to_string(&config).unwrap();
    write(
        &config,
        &text.replace("LOG_LEVEL", "GITHUB_TOKEN = \"filled\"\nLOG_LEVEL"),
    );
    assert_eq!(drift(), Some(Drift::Same), "the user gave it the value");
    write(
        &config,
        &text.replace(
            "[mcp_servers.github]",
            "[mcp_servers.github]\nenabled = false",
        ),
    );
    assert_eq!(
        drift(),
        Some(Drift::Needs(vec!["GITHUB_TOKEN".into()])),
        "a switch is not a change"
    );

    let mut changed = github();
    changed["env"]["LOG_LEVEL"] = "info".into();
    claude_server(&fixture, changed);
    assert_eq!(drift(), Some(Drift::Differs), "the source was edited");

    let mut local = pair.clone();
    local.from.scope = Scope::Local;
    local.project = fixture.root.join("elsewhere");
    assert_eq!(
        super::drift(&local, &fixture.claude, &fixture.codex, &fixture.project),
        None,
        "another project's local server is not this one's"
    );

    fs::remove_file(&config).unwrap();
    assert_eq!(
        drift(),
        Some(Drift::Gone),
        "a copy that is gone is forgotten"
    );
}

#[test]
fn codex_variables_passed_by_name_become_claude_references_and_back() {
    let fixture = fixture();
    write(
        &fixture.root.join("codex/config.toml"),
        "[mcp_servers.remote]\nurl = \"https://m.example/mcp\"\nbearer_token_env_var = \"API\"\nstartup_timeout_sec = 20\n\n[mcp_servers.remote.http_headers]\nX-Team = \"core\"\n\n[mcp_servers.remote.env_http_headers]\nX-Key = \"KEY_VAR\"\n",
    );
    let item = fixture.item(&fixture.codex, "remote");
    let to_claude = preview(
        &item,
        &fixture.codex,
        fixture.to(&fixture.claude),
        &fixture.project,
        &[],
    )
    .unwrap();
    let Change::AddDefinition {
        definition,
        scope,
        name,
    } = &to_claude.change
    else {
        panic!("a server copy adds a definition");
    };
    assert_eq!((name.as_str(), *scope), ("remote", Scope::User));
    assert_eq!(
        *definition,
        json!({"type": "http", "url": "https://m.example/mcp", "headers": {"X-Team": "core", "X-Key": "${KEY_VAR}", "Authorization": "Bearer ${API}"}})
    );
    assert!(to_claude.secrets.is_empty(), "references hold no secret");
    assert_eq!(
        translate(definition, Provider::Claude, Provider::Codex).unwrap(),
        json!({"url": "https://m.example/mcp", "bearer_token_env_var": "API", "http_headers": {"X-Team": "core"}, "env_http_headers": {"X-Key": "KEY_VAR"}}),
        "only the switch-like timeout, which Claude has no place for, stays behind"
    );
}

#[test]
fn plugins_sse_servers_and_names_in_use_are_not_copied() {
    let fixture = fixture();
    write(
        &fixture.root.join("claude/settings.json"),
        r#"{"enabledPlugins": {"tool@market": true}}"#,
    );
    write(
        &fixture.root.join("claude/.claude.json"),
        r#"{"mcpServers": {"events": {"type": "sse", "url": "https://e.example/sse"}, "github": {"command": "npx", "headers": {"Authorization": "Bearer abc"}}}}"#,
    );
    write(
        &fixture.root.join("codex/config.toml"),
        "[mcp_servers.github]\ncommand = \"gh\"\n",
    );
    let copy = |name: &str| {
        let item = fixture.item(&fixture.claude, name);
        preview(
            &item,
            &fixture.claude,
            fixture.to(&fixture.codex),
            &fixture.project,
            &[],
        )
        .map(|preview| preview.text)
    };
    assert_eq!(copy("tool@market"), Err("Claude only".into()));
    assert_eq!(copy("events"), Err("Codex can't use SSE servers.".into()));
    assert_eq!(
        copy("github"),
        Err("A server named github is already there.".into())
    );
}

#[test]
fn a_bearer_token_header_is_a_secret_whatever_its_name() {
    let headers = json!({"headers": {"X-Custom": "Bearer abcdef123456", "X-Ref": "Bearer ${TOKEN}", "X-Plain": "on"}});
    let found: Vec<String> = secrets(&headers, Provider::Claude)
        .into_iter()
        .map(|secret| secret.label)
        .collect();
    assert_eq!(found, ["X-Custom"]);
}

#[test]
fn a_skill_folder_copies_with_its_files_and_flags_a_later_edit() {
    let fixture = fixture();
    let folder = fixture.root.join("claude/skills/review");
    write(
        &folder.join("SKILL.md"),
        "---\nname: review\ndescription: Reviews\n---\nBody.\n",
    );
    write(&folder.join("scripts/run.sh"), "echo hi\n");
    let item = fixture.item(&fixture.claude, "review");
    let made = preview(
        &item,
        &fixture.claude,
        fixture.to(&fixture.codex),
        &fixture.project,
        &[],
    )
    .unwrap();
    assert_eq!(made.to, fixture.root.join("codex/skills/review"));
    assert_eq!(made.text, "SKILL.md\nscripts/run.sh");
    let pair = made.pair.clone();
    fixture.copy(&fixture.codex, made);
    assert_eq!(
        fs::read_to_string(fixture.root.join("codex/skills/review/scripts/run.sh")).unwrap(),
        "echo hi\n"
    );
    assert!(pair.to.is(&fixture.item(&fixture.codex, "review")));
    let drift = || drift(&pair, &fixture.claude, &fixture.codex, &fixture.project);
    assert_eq!(drift(), Some(Drift::Same));
    write(&folder.join("scripts/run.sh"), "echo changed\n");
    assert_eq!(drift(), Some(Drift::Differs));
    assert_eq!(
        preview(
            &item,
            &fixture.claude,
            fixture.to(&fixture.codex),
            &fixture.project,
            &[]
        )
        .map(|preview| preview.text),
        Err("A skill named review is already there.".into())
    );
}
#[test]
fn copies_are_remembered_across_restarts_and_a_copy_over_one_replaces_it() {
    let fixture = fixture();
    let path = fixture.root.join("keystone.sqlite3");
    let end = |name: &str| End {
        provider: Provider::Codex,
        account: AccountId::new("a"),
        kind: Kind::McpServer,
        scope: Scope::User,
        name: name.into(),
        source: fixture.root.join("codex/config.toml"),
    };
    let pair = |left_out: &[&str]| Pair {
        from: End {
            provider: Provider::Claude,
            ..end("github")
        },
        to: end("github"),
        left_out: left_out.iter().map(|label| (*label).to_owned()).collect(),
        project: fixture.project.clone(),
    };
    let database = crate::database::Database::open(&path).unwrap();
    database.record_copy(&pair(&["GITHUB_TOKEN"])).unwrap();
    database.record_copy(&pair(&[])).unwrap();
    drop(database);
    let reopened = crate::database::Database::open(&path).unwrap();
    assert_eq!(reopened.copies().unwrap(), [pair(&[])]);
}

fn labels(definition: &Value) -> Vec<String> {
    secrets(definition, Provider::Claude)
        .into_iter()
        .map(|secret| secret.label)
        .collect()
}

#[test]
fn secrets_in_arguments_and_urls_are_masked_and_left_out_unless_included() {
    let fixture = fixture();
    let global = json!({"mcpServers": {
        "pg": {"command": "npx", "args": ["-y", "@x/server-postgres", "postgresql://u:pw-secret@h/db"]},
        "remote": {"command": "npx", "args": ["mcp-remote", "https://x/mcp", "--header", "Authorization: Bearer live-secret"]},
        "query": {"type": "http", "url": "https://m.example/mcp?api_key=q-secret&team=core"}
    }});
    write(
        &fixture.root.join("claude/.claude.json"),
        &global.to_string(),
    );
    let copy = |name: &str, include: &[String]| {
        let item = fixture.item(&fixture.claude, name);
        preview(
            &item,
            &fixture.claude,
            fixture.to(&fixture.codex),
            &fixture.project,
            include,
        )
        .unwrap()
    };
    let pg = copy("pg", &[]);
    assert_eq!(pg.secrets, ["Argument 3"]);
    assert!(
        !pg.text.contains("pw-secret") && !pg.text.contains("postgresql"),
        "{}",
        pg.text
    );
    let pg = copy("pg", &["Argument 3".into()]);
    assert!(
        pg.text.contains(&format!("\"{MASK}\"")) && !pg.text.contains("pw-secret"),
        "{}",
        pg.text
    );
    let Change::AddDefinition { definition, .. } = &pg.change else {
        panic!()
    };
    assert_eq!(
        definition["args"][2], "postgresql://u:pw-secret@h/db",
        "an included secret is written"
    );

    let remote = copy("remote", &[]);
    assert_eq!(remote.secrets, ["Argument 4"]);
    assert!(!remote.text.contains("live-secret"), "{}", remote.text);

    let query = copy("query", &[]);
    assert_eq!(query.secrets, ["URL"]);
    assert!(
        query
            .text
            .contains("url = \"https://m.example/mcp?team=core\""),
        "{}",
        query.text
    );
    let query = copy("query", &["URL".into()]);
    assert!(
        query.text.contains(&format!("api_key={MASK}&team=core")),
        "{}",
        query.text
    );
}

#[test]
fn names_say_secret_by_their_parts_and_switches_take_no_value() {
    assert_eq!(
        labels(
            &json!({"env": {"DB_PASS": "a", "AWS_CREDENTIALS": "b", "OAUTH": "c", "AUTHOR": "d", "KEYMAP": "e"},
                        "headers": {"Cookie": "f", "Authorization": "Basic Zm9v"}})
        ),
        [
            "AWS_CREDENTIALS",
            "DB_PASS",
            "OAUTH",
            "Authorization",
            "Cookie"
        ]
    );
    assert!(
        labels(&json!({"args": ["--no-auth", "server.js", "--author", "x", "--keymap", "f"]}))
            .is_empty()
    );
    assert_eq!(
        labels(&json!({"args": ["--apikey", "v", "--token=t"]})),
        ["--apikey", "--token"]
    );
    assert_eq!(
        labels(
            &json!({"env": {"GITHUB_TOKEN": "${GITHUB_TOKEN}", "DSN": "postgres://u:p@h/db"}, "headers": {"X-Custom": "Bearer abcdef123456", "X-Ref": "bearer ${TOKEN}", "X-Plain": "on"}})
        ),
        ["DSN", "X-Custom"]
    );
}

#[test]
fn references_codex_cannot_expand_and_settings_claude_lacks_are_refused() {
    let fixture = fixture();
    write(
        &fixture.root.join("claude/.claude.json"),
        r#"{"mcpServers": {"gh": {"command": "npx", "env": {"GITHUB_TOKEN": "${GH_PAT}"}}}}"#,
    );
    write(
        &fixture.root.join("codex/config.toml"),
        "[mcp_servers.tools]\ncommand = \"tool\"\ndisabled_tools = [\"delete_repo\"]\n",
    );
    let refused = |from: &Location, to: &Location, name: &str| {
        let item = fixture.item(from, name);
        preview(&item, from, fixture.to(to), &fixture.project, &[]).map(|preview| preview.text)
    };
    assert_eq!(
        refused(&fixture.claude, &fixture.codex, "gh"),
        Err("Codex can't expand the reference in GITHUB_TOKEN.".into())
    );
    assert_eq!(
        refused(&fixture.codex, &fixture.claude, "tools"),
        Err("Claude has no disabled_tools.".into())
    );
}

#[test]
fn a_secret_filled_by_reference_and_codex_rewriting_numbers_are_not_drift() {
    let fixture = fixture();
    claude_server(&fixture, github());
    let item = fixture.item(&fixture.claude, "github");
    let made = preview(
        &item,
        &fixture.claude,
        fixture.to(&fixture.codex),
        &fixture.project,
        &["--api-key".into()],
    )
    .unwrap();
    let pair = made.pair.clone();
    fixture.copy(&fixture.codex, made);
    let config = fixture.root.join("codex/config.toml");
    let text = fs::read_to_string(&config).unwrap();
    write(
        &config,
        &text.replace(
            "env_vars = [\"HOME_DIR\"]",
            "env_vars = [\"HOME_DIR\", \"GITHUB_TOKEN\"]",
        ),
    );
    assert_eq!(
        drift(&pair, &fixture.claude, &fixture.codex, &fixture.project),
        Some(Drift::Same),
        "the user passed the token by name"
    );

    let other = Location::Codex {
        home: fixture.root.join("codex-work"),
        user_home: None,
    };
    write(
        &config,
        "[mcp_servers.slow]\ncommand = \"node\"\nstartup_timeout_sec = 20\nenabled = false\n",
    );
    let slow = fixture.item(&fixture.codex, "slow");
    let made = preview(
        &slow,
        &fixture.codex,
        fixture.to(&other),
        &fixture.project,
        &[],
    )
    .unwrap();
    assert_eq!(
        made.text, "[mcp_servers.slow]\ncommand = \"node\"\nstartup_timeout_sec = 20\n",
        "a copy arrives on"
    );
    let pair = made.pair.clone();
    fixture.copy(&other, made);
    let copied = fixture.root.join("codex-work/config.toml");
    write(
        &copied,
        &fs::read_to_string(&copied)
            .unwrap()
            .replace("= 20", "= 20.0"),
    );
    assert_eq!(
        drift(&pair, &fixture.codex, &other, &fixture.project),
        Some(Drift::Same)
    );
    fs::remove_file(&copied).unwrap();
    assert_eq!(
        drift(&pair, &fixture.codex, &other, &fixture.project),
        Some(Drift::Gone)
    );
}

#[cfg(unix)]
#[test]
fn links_in_a_skill_are_listed_and_copied_as_links() {
    let fixture = fixture();
    let folder = fixture.root.join("claude/skills/linked");
    write(&folder.join("SKILL.md"), "---\nname: linked\n---\n");
    write(&fixture.root.join("credentials"), "secret\n");
    std::os::unix::fs::symlink(fixture.root.join("credentials"), folder.join("creds")).unwrap();
    std::os::unix::fs::symlink(".", folder.join("up")).unwrap();
    let item = fixture.item(&fixture.claude, "linked");
    let made = preview(
        &item,
        &fixture.claude,
        fixture.to(&fixture.codex),
        &fixture.project,
        &[],
    )
    .unwrap();
    assert_eq!(
        made.text, "SKILL.md\ncreds\nup",
        "a link to a folder is not followed"
    );
    let pair = made.pair.clone();
    fixture.copy(&fixture.codex, made);
    let copied = fixture.root.join("codex/skills/linked/creds");
    assert!(
        fs::symlink_metadata(&copied)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        drift(&pair, &fixture.claude, &fixture.codex, &fixture.project),
        Some(Drift::Same)
    );
}

#[cfg(unix)]
#[test]
fn a_skill_whose_relative_link_leads_out_of_its_folder_is_not_copied() {
    let fixture = fixture();
    let outside = fixture.root.join("claude/skills/outside");
    write(&outside.join("SKILL.md"), "---\nname: outside\n---\n");
    std::os::unix::fs::symlink("../../../credentials", outside.join("creds")).unwrap();
    let item = fixture.item(&fixture.claude, "outside");
    assert_eq!(
        preview(
            &item,
            &fixture.claude,
            fixture.to(&fixture.codex),
            &fixture.project,
            &[]
        )
        .map(|preview| preview.text),
        Err("outside links outside its folder.".into())
    );
}

#[test]
fn filling_the_only_secret_by_reference_unreadable_files_and_moved_arguments_are_not_drift() {
    let fixture = fixture();
    write(
        &fixture.root.join("claude/.claude.json"),
        r#"{"mcpServers": {"api": {"command": "node", "args": ["s.js", "sk-first-0123456789abcd", "ghp_second0123456789abcd"], "env": {"API_KEY": "k"}}}}"#,
    );
    let item = fixture.item(&fixture.claude, "api");
    let made = preview(
        &item,
        &fixture.claude,
        fixture.to(&fixture.codex),
        &fixture.project,
        &["Argument 3".into()],
    )
    .unwrap();
    assert_eq!(made.pair.left_out, ["API_KEY", "Argument 2"]);
    let pair = made.pair.clone();
    fixture.copy(&fixture.codex, made);
    let drift = || drift(&pair, &fixture.claude, &fixture.codex, &fixture.project);
    assert_eq!(
        drift(),
        Some(Drift::Needs(vec!["API_KEY".into(), "Argument 2".into()]))
    );

    let config = fixture.root.join("codex/config.toml");
    let text = fs::read_to_string(&config).unwrap();
    let filled = text.replace(
        "command = \"node\"",
        "command = \"node\"\nenv_vars = [\"API_KEY\"]",
    );
    write(&config, &filled);
    assert_eq!(
        drift(),
        Some(Drift::Needs(vec!["Argument 2".into()])),
        "the only env entry, filled by name"
    );

    write(&config, "[mcp_servers.api\n");
    assert_eq!(drift(), None, "a file mid-edit is not a gone copy");
}

#[test]
fn a_masked_query_value_left_out_is_not_shown_and_env_style_arguments_are_secrets() {
    let fixture = fixture();
    write(
        &fixture.root.join("claude/.claude.json"),
        r#"{"mcpServers": {"q": {"type": "http", "url": "https://m.example/mcp?k=sk-abcdef0123456789xyz&team=core"}}}"#,
    );
    let item = fixture.item(&fixture.claude, "q");
    let made = preview(
        &item,
        &fixture.claude,
        fixture.to(&fixture.codex),
        &fixture.project,
        &[],
    )
    .unwrap();
    assert!(
        made.text
            .contains("url = \"https://m.example/mcp?team=core\""),
        "{}",
        made.text
    );
    assert_eq!(
        labels(
            &json!({"args": ["GITHUB_TOKEN=abc", "node", "MODE=basic setup"], "env": {"BYPASS": "1", "MODE": "basic setup"}})
        ),
        ["GITHUB_TOKEN"]
    );
}

#[test]
fn query_credentials_in_url_arguments_literal_codex_references_and_refilled_arguments() {
    let fixture = fixture();
    write(
        &fixture.root.join("claude/.claude.json"),
        r#"{"mcpServers": {"remote": {"command": "npx", "args": ["mcp-remote", "https://m.example/mcp?api_key=opaque-credential-12345"]}, "api": {"command": "node", "args": ["s.js", "sk-example0123456789abcdef"]}}}"#,
    );
    let item = fixture.item(&fixture.claude, "remote");
    let made = preview(
        &item,
        &fixture.claude,
        fixture.to(&fixture.codex),
        &fixture.project,
        &[],
    )
    .unwrap();
    assert_eq!(made.secrets, ["Argument 2"]);
    assert!(!made.text.contains("opaque-credential"), "{}", made.text);

    write(
        &fixture.root.join("codex/config.toml"),
        "[mcp_servers.literal]\ncommand = \"tool\"\n\n[mcp_servers.literal.env]\nTEMPLATE = \"${HOME}\"\n",
    );
    let item = fixture.item(&fixture.codex, "literal");
    assert_eq!(
        preview(
            &item,
            &fixture.codex,
            fixture.to(&fixture.claude),
            &fixture.project,
            &[]
        )
        .map(|preview| preview.text),
        Err("Claude would expand the ${…} in TEMPLATE.".into())
    );

    let item = fixture.item(&fixture.claude, "api");
    let made = preview(
        &item,
        &fixture.claude,
        fixture.to(&fixture.codex),
        &fixture.project,
        &[],
    )
    .unwrap();
    let pair = made.pair.clone();
    fixture.copy(&fixture.codex, made);
    let drift = || drift(&pair, &fixture.claude, &fixture.codex, &fixture.project);
    assert_eq!(drift(), Some(Drift::Needs(vec!["Argument 2".into()])));
    let config = fixture.root.join("codex/config.toml");
    let text = fs::read_to_string(&config).unwrap();
    write(
        &config,
        &text.replace(
            "args = [\"s.js\"]",
            "args = [\"s.js\", \"sk-example0123456789abcdef\"]",
        ),
    );
    assert_eq!(drift(), Some(Drift::Same), "the user put the argument back");
}
