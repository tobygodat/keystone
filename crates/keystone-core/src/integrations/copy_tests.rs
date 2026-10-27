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
    assert!(
        labels(
            &json!({"args": ["--config", r#"{"apiKey":"${API_KEY}","token":"","debug":true}"#]})
        )
        .is_empty()
    );
    assert_eq!(
        labels(
            &json!({"env": {"GITHUB_TOKEN": "${GITHUB_TOKEN}", "NPM_TOKEN": "$NPM_TOKEN", "DSN": "postgres://u:p@h/db"}, "headers": {"X-Custom": "Bearer abcdef123456", "X-Ref": "bearer ${TOKEN}", "X-Var": "Bearer $GITHUB_TOKEN", "X-Plain": "on"}})
        ),
        ["DSN", "NPM_TOKEN", "X-Custom"]
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

/// Realistic MCP server URLs, each with the secret its path holds, if any.
const URLS: &[(&str, Option<&str>)] = &[
    (
        "https://hooks.slack.com/services/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",
        Some("aB3dE5fG7hJ9kL1mN3pQ5rS7"),
    ),
    (
        "https://mcp.zapier.com/api/mcp/s/NjA5ZjQ0MGQtZjk2Ny00YjE2LWE5ZjQtM2Q4ZjE2ZTJhYjU0/mcp",
        Some("NjA5ZjQ0MGQtZjk2Ny00YjE2LWE5ZjQtM2Q4ZjE2ZTJhYjU0"),
    ),
    (
        "https://actions.zapier.com/mcp/sk-ak-4f8a9b2c3d1e5f6a7b8c9d0e/sse",
        Some("sk-ak-4f8a9b2c3d1e5f6a7b8c9d0e"),
    ),
    (
        "https://hook.eu1.make.com/ab12cd34ef56gh78ij90kl12mn34op56",
        Some("ab12cd34ef56gh78ij90kl12mn34op56"),
    ),
    (
        "https://mcp.pipedream.net/3f1c2b4a-9d8e-4f7a-b6c5-1a2b3c4d5e6f/gmail",
        Some("3f1c2b4a-9d8e-4f7a-b6c5-1a2b3c4d5e6f"),
    ),
    (
        "https://n8n.example.com/mcp/5b2e7c1a-0d3f-4e8b-9a6c-7f1e2d3c4b5a/sse",
        Some("5b2e7c1a-0d3f-4e8b-9a6c-7f1e2d3c4b5a"),
    ),
    (
        "https://discord.com/api/webhooks/123456789012345678/AbCdEfGhIjKlMnOpQrStUvWxYz0123456789-_AbCdEfGhIjKlMnOpQrStUvWxYz01",
        Some("AbCdEfGhIjKlMnOpQrStUvWxYz0123456789-_AbCdEfGhIjKlMnOpQrStUvWxYz01"),
    ),
    (
        "https://api.telegram.org/bot123456789:AAHfiqksKZ8WmR2zSjiQ7_v4TMAKdiHm9T0/getMe",
        Some("AAHfiqksKZ8WmR2zSjiQ7_v4TMAKdiHm9T0"),
    ),
    (
        "https://example.com/mcp/ghp_16C7e42F292c6912E7710c838347Ae178B4a/sse",
        Some("ghp_16C7e42F292c6912E7710c838347Ae178B4a"),
    ),
    (
        "https://script.google.com/macros/s/AKfycbz3Xk9Qm2Lp7Rt5Vw8Yb1Nc4Df6Gh0Jk/exec",
        Some("AKfycbz3Xk9Qm2Lp7Rt5Vw8Yb1Nc4Df6Gh0Jk"),
    ),
    (
        "https://maker.ifttt.com/trigger/mcp/with/key/dK3xQ9mL2pR7tW5vB8nY1c",
        Some("dK3xQ9mL2pR7tW5vB8nY1c"),
    ),
    (
        "https://mcp.composio.dev/composio/server/0b7e1c2d-3f4a-4b5c-8d6e-9f0a1b2c3d4e/mcp?include_composio_helper_actions=true",
        Some("0b7e1c2d-3f4a-4b5c-8d6e-9f0a1b2c3d4e"),
    ),
    (
        "https://hooks.slack.com/services/T0123ABCD/B0456EFGH/aB3dE5fG7hJ9%6BL1mN3pQ5rS7",
        Some("aB3dE5fG7hJ9%6BL1mN3pQ5rS7"),
    ),
    (
        "https://mcp.zapier.com/api/mcp/s/NjA5ZjQ0MGQtZjk2Ny00YjE2LWE5ZjQtM2Q4ZjE2ZTJhYjU0=/mcp",
        Some("NjA5ZjQ0MGQtZjk2Ny00YjE2LWE5ZjQtM2Q4ZjE2ZTJhYjU0="),
    ),
    (
        "https://cloud.activepieces.com/api/v1/mcp/Ab3dE5fG7_J9kL1mN3pQ5/sse",
        Some("Ab3dE5fG7_J9kL1mN3pQ5"),
    ),
    (
        "https://ghp_16C7e42F292c6912E7710c838347Ae178B4a@github.com/org/private-mcp",
        Some("ghp_16C7e42F292c6912E7710c838347Ae178B4a"),
    ),
    (
        "https://ghp_16C7e42F292c6912E7710c838347Ae178B4a:x-oauth-basic@github.com/org/mcp",
        Some("ghp_16C7e42F292c6912E7710c838347Ae178B4a x-oauth-basic"),
    ),
    (
        "https://ghp_16C7e42F292c6912E7710c838347Ae178B4a:@github.com/org/mcp",
        Some("ghp_16C7e42F292c6912E7710c838347Ae178B4a"),
    ),
    (
        "https://mcp.example.com/s/V1StGXR8_Z5jdHi6B-myT/mcp",
        Some("V1StGXR8_Z5jdHi6B-myT"),
    ),
    (
        "https://mcp.example.com/s/Ab3d_E5fG_J9kL1mN3pQ5/mcp",
        Some("Ab3d_E5fG_J9kL1mN3pQ5"),
    ),
    (
        "https://mcp.example.com/s/Ab3dE5fG7%2FJ9kL1mN3pQ5=/mcp",
        Some("Ab3dE5fG7%2FJ9kL1mN3pQ5="),
    ),
    (
        "https://mcp.example.com/s/Ab3dE5fG7%2BJ9kL1mN3pQ5/mcp",
        Some("Ab3dE5fG7%2BJ9kL1mN3pQ5"),
    ),
    ("https://mcp.linear.app/sse", None),
    ("https://mcp.notion.com/mcp", None),
    ("https://api.githubcopilot.com/mcp/", None),
    ("https://mcp.atlassian.com/v1/sse", None),
    (
        "https://mcp.supabase.com/mcp?project_ref=abcdefghijklmnopqrst",
        None,
    ),
    ("http://localhost:3000/mcp", None),
    ("http://127.0.0.1:8080/v1/sse", None),
    ("https://my-mcp-server.username.workers.dev/sse", None),
    ("https://abc123def456ghi789jkl.ngrok-free.app/mcp", None),
    (
        "https://api.example.com/v1/projects/1234567890123456/mcp",
        None,
    ),
    ("https://mcp.example.com/2025-03-26/mcp", None),
    ("https://server.smithery.ai/@smithery-ai/github/mcp", None),
    (
        "https://mcp.example.com/model-context-protocol-server-2025/mcp",
        None,
    ),
    ("https://mcp.example.com/oauth/authorize", None),
    ("https://mcp.example.com/auth/callback", None),
    ("https://mcp.example.com/tokens/list", None),
    (
        "https://gitlab.example.com/api/v4/projects/group%2Fproject/mcp",
        None,
    ),
    (
        "https://raw.githubusercontent.com/modelcontextprotocol/servers/main/README.md",
        None,
    ),
    (
        "https://unpkg.com/@modelcontextprotocol/server-everything@2025.4.8/dist/index.js",
        None,
    ),
    (
        "https://api.example.com/repos/Microsoft/TypeScript-Website/mcp",
        None,
    ),
    (
        "https://mcp.example.com/v2/servers/github-copilot-enterprise/sse",
        None,
    ),
    ("https://mcp.example.com/~alice/mcp", None),
    (
        "https://mcp.example.com/models/meta-llama/Llama-4-Scout-17B-16E-Instruct/mcp",
        None,
    ),
    (
        "https://huggingface.co/deepseek-ai/DeepSeek-R1-Distill-Qwen-32B",
        None,
    ),
    ("https://alice@mcp.example.com/mcp", None),
    (
        "https://mcp.example.com/models/mistralai/Mistral-Large-Instruct-2411/mcp",
        None,
    ),
    (
        "https://mcp.example.com/models/mistralai/Magistral-Medium-2506/mcp",
        None,
    ),
    (
        "https://huggingface.co/Qwen/Qwen2.5-Coder-32B-Instruct",
        None,
    ),
    (
        "https://huggingface.co/microsoft/Phi-3-mini-4k-instruct",
        None,
    ),
    (
        "https://mcp.example.com/mcp#access_token=ghp_16C7e42F292c6912E7710c838347Ae178B4a",
        Some("ghp_16C7e42F292c6912E7710c838347Ae178B4a"),
    ),
    (
        "https://mcp.example.com/mcp?region=us#token=opaqueCredential123&view=full",
        Some("opaqueCredential123"),
    ),
    ("https://docs.example.com/page#section-2", None),
];

/// Realistic stdio commands, each with the secret it holds, if any.
const COMMANDS: &[(&str, Option<&str>)] = &[
    (
        "GITHUB_PERSONAL_ACCESS_TOKEN=ghp_16C7e42F292c6912E7710c838347Ae178B4a npx -y @modelcontextprotocol/server-github",
        Some("ghp_16C7e42F292c6912E7710c838347Ae178B4a"),
    ),
    (
        "docker run -i --rm -e GITHUB_TOKEN=ghp_16C7e42F292c6912E7710c838347Ae178B4a ghcr.io/github/github-mcp-server",
        Some("ghp_16C7e42F292c6912E7710c838347Ae178B4a"),
    ),
    (
        "/usr/local/bin/mcp-server --api-key=abcd1234",
        Some("abcd1234"),
    ),
    (
        "npx -y @smithery/cli run @x/y --key 1234abcd5678",
        Some("1234abcd5678"),
    ),
    (
        "uvx mcp-server-postgres postgresql://admin:hunter2@db.example.com/app",
        Some("hunter2"),
    ),
    (
        "npx mcp-remote https://hooks.slack.com/services/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",
        Some("aB3dE5fG7hJ9kL1mN3pQ5rS7"),
    ),
    (
        "env API_KEY=sk-proj-abc123def456ghi789 node server.js",
        Some("sk-proj-abc123def456ghi789"),
    ),
    (
        "node server.js --token ghp_16C7e42F292c6912E7710c838347Ae178B4a",
        Some("ghp_16C7e42F292c6912E7710c838347Ae178B4a"),
    ),
    (
        "npx mcp-remote https://example.com/mcp --header \"Authorization: Bearer opaquecredential123\"",
        Some("opaquecredential123"),
    ),
    (
        "node server.js --password \"correct horse battery staple\"",
        Some("correct horse battery staple"),
    ),
    (
        "npx mcp-remote https://example.com/mcp --header Authorization: Bearer 9f8e7d6c5b4a39281706f5e4d3c2b1a0",
        Some("9f8e7d6c5b4a39281706f5e4d3c2b1a0"),
    ),
    (
        "uvx --from git+https://ghp_16C7e42F292c6912E7710c838347Ae178B4a@github.com/org/private-mcp server",
        Some("ghp_16C7e42F292c6912E7710c838347Ae178B4a"),
    ),
    (
        "node server.js --auth Bearer 9f8e7d6c5b4a39281706f5e4d3c2b1a0",
        Some("9f8e7d6c5b4a39281706f5e4d3c2b1a0"),
    ),
    (
        "uvx --from git+https://ghp_16C7e42F292c6912E7710c838347Ae178B4a:x-oauth-basic@github.com/org/mcp server",
        Some("ghp_16C7e42F292c6912E7710c838347Ae178B4a x-oauth-basic"),
    ),
    (
        "uvx --from git+https://ghp_16C7e42F292c6912E7710c838347Ae178B4a:@github.com/org/mcp server",
        Some("ghp_16C7e42F292c6912E7710c838347Ae178B4a"),
    ),
    (
        "node s.js --password correct\\ horse\\ battery\\ staple",
        Some("correct horse battery staple"),
    ),
    (
        "npx mcp-remote https://example.com/mcp --header Authorization:\\ Bearer\\ abcdefgh1234",
        Some("abcdefgh1234"),
    ),
    (
        "node s.js --title a\\\"b --api-key=abcd1234",
        Some("abcd1234"),
    ),
    (
        "npx mcp-remote https://example.com/mcp --header Authorization: Basic dXNlcjpwYXNzd29yZA==",
        Some("dXNlcjpwYXNzd29yZA=="),
    ),
    ("npx", None),
    ("/usr/bin/python3", None),
    ("uvx mcp-server-fetch", None),
    ("node /home/u/token-server/index.js", None),
    (
        "docker run -i --rm -e GITHUB_TOKEN ghcr.io/github/github-mcp-server",
        None,
    ),
    ("npx -y @keymap/mcp --no-auth", None),
    ("C:\\Program Files\\nodejs\\npx.cmd", None),
    ("/Applications/Tools.app/Contents/MacOS/mcp-server", None),
    ("bun run --hot server.ts", None),
    (
        "npx  -y   @modelcontextprotocol/server-filesystem /Users/me/Documents",
        None,
    ),
    ("uv run --with mcp[cli] mcp run server.py", None),
    ("java -jar /opt/mcp/server-1.2.3.jar --port 8080", None),
    ("npx -y @upstash/context7-mcp@latest", None),
    ("npx -y mcp-remote https://mcp.linear.app/sse", None),
    (
        "python -m mcp_server_git --repository /home/u/src/keystone",
        None,
    ),
    (
        "node '/Users/me/MCP Servers/notes/index.js' --name \"Team notes\"",
        None,
    ),
    ("node server.js --profile basic analytics", None),
    (
        "npx mcp-remote https://example.com/mcp --header \"Authorization: Bearer $GITHUB_TOKEN\"",
        None,
    ),
    (
        "npx mcp-remote https://example.com/mcp --header Authorization: Bearer $GITHUB_TOKEN",
        None,
    ),
    (
        "npx mcp-remote https://mcp.example.com/mcp --header 'Authorization:Bearer opaqueCredential123'",
        Some("opaqueCredential123"),
    ),
    (
        "npx mcp-remote https://mcp.example.com/mcp --header Authorization:Bearer opaqueCredential123",
        Some("opaqueCredential123"),
    ),
    (
        "npx mcp-remote https://mcp.example.com/mcp --header 'Authorization:Basic dXNlcjpwYXNzd29yZA=='",
        Some("dXNlcjpwYXNzd29yZA=="),
    ),
    (
        r#"npx -y @smithery/cli run @x/y --config '{"apiKey":"opaqueCredential123"}'"#,
        Some("opaqueCredential123"),
    ),
    (
        r#"npx -y @smithery/cli run @x/y --config '{"debug":true,"region":"us-east-1"}'"#,
        None,
    ),
];

/// Realistic stdio arguments, each with the secret the last one holds, if any.
const ARGS: &[(&[&str], Option<&str>)] = &[
    (
        &[
            "mcp-remote",
            "https://mcp.example.com/mcp",
            "--header",
            "Authorization:Bearer opaqueCredential123",
        ],
        Some("opaqueCredential123"),
    ),
    (
        &[
            "-y",
            "@smithery/cli",
            "run",
            "@x/y",
            "--config",
            r#"{"apiKey":"opaqueCredential123"}"#,
        ],
        Some("opaqueCredential123"),
    ),
    (
        &[
            "-y",
            "@smithery/cli",
            "run",
            "@x/y",
            "--config",
            r#"{"auth":{"token":"opaqueCredential123"}}"#,
        ],
        Some("opaqueCredential123"),
    ),
    (
        &[
            "-y",
            "@smithery/cli",
            "run",
            "@x/y",
            "--config",
            r#"{"debug":true,"region":"us-east-1"}"#,
        ],
        None,
    ),
    (
        &[
            "mcp-remote",
            "https://mcp.example.com/mcp#access_token=opaqueCredential123",
        ],
        Some("opaqueCredential123"),
    ),
    (
        &["mcp-remote", "https://docs.example.com/page#section-2"],
        None,
    ),
];

#[test]
fn secrets_in_url_paths_and_commands_are_masked_and_names_versions_and_ids_are_not() {
    let fixture = fixture();
    let urls = URLS.iter().map(|(url, secret)| {
        (
            json!({"type": "http", "url": url}),
            "url",
            "URL".to_owned(),
            json!(url),
            *secret,
        )
    });
    let commands = COMMANDS.iter().map(|(command, secret)| {
        let server = json!({"type": "stdio", "command": command});
        (
            server,
            "command",
            "Command".to_owned(),
            json!(command),
            *secret,
        )
    });
    let args = ARGS.iter().map(|(args, secret)| {
        let server = json!({"type": "stdio", "command": "npx", "args": args});
        let label = format!("Argument {}", args.len());
        (server, "args", label, json!(args), *secret)
    });
    for (server, field, label, given, secret) in urls.chain(commands).chain(args) {
        claude_server(&fixture, server);
        let item = fixture.item(&fixture.claude, "github");
        let copy = |include: &[String]| {
            preview(
                &item,
                &fixture.claude,
                fixture.to(&fixture.codex),
                &fixture.project,
                include,
            )
            .unwrap()
        };
        let written = |preview: &Preview| match &preview.change {
            Change::AddDefinition { definition, .. } => definition[field].clone(),
            change => panic!("a server copy adds a definition: {change:?}"),
        };
        let (left_out, included) = (copy(&[]), copy(std::slice::from_ref(&label)));
        let Some(secret) = secret else {
            assert!(
                left_out.secrets.is_empty(),
                "{given}: {:?}",
                left_out.secrets
            );
            assert_eq!(written(&left_out), given, "copied as it is");
            continue;
        };
        let hidden = |text: &str| secret.split_whitespace().all(|part| !text.contains(part));
        assert_eq!(left_out.secrets, [label], "{given}");
        for preview in [&left_out, &included] {
            assert!(hidden(&preview.text), "{given}: {}", preview.text);
        }
        assert!(included.text.contains(MASK), "{given}: {}", included.text);
        assert!(
            hidden(&written(&left_out).to_string()),
            "{given} is copied without its secret"
        );
        assert_eq!(written(&included), given, "an included secret is written");
    }
}

#[test]
fn a_backslash_outside_quotes_escapes_the_next_character_as_a_shell_reads_it() {
    let cases = [
        ("a\\ b c", &[("a b", 0..4), ("c", 5..6)]),
        ("a\\\"b \"c d\"", &[("a\"b", 0..4), ("c d", 5..10)]),
        ("a\\\nb c", &[("ab", 0..4), ("c", 5..6)]),
        ("a \\\n b", &[("a", 0..1), ("b", 5..6)]),
        ("a b\\", &[("a", 0..1), ("b\\", 2..4)]),
        ("'a\\ b' \"c\\ d\"", &[("a\\ b", 0..6), ("c\\ d", 7..13)]),
    ];
    for (command, words) in cases {
        let words: Vec<(String, std::ops::Range<usize>)> = words
            .iter()
            .map(|(word, span)| (word.to_string(), span.clone()))
            .collect();
        assert_eq!(shell_words(command), words, "{command:?}");
    }
}

#[test]
fn a_path_or_command_secret_shows_masked_is_left_out_and_is_needed_until_filled() {
    let fixture = fixture();
    let config = fixture.root.join("codex/config.toml");
    let hook = "https://hooks.slack.com/services/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";
    let command = "env  API_KEY=sk-proj-abc123def456ghi789   node server.js --token ghp_16C7e42F292c6912E7710c838347Ae178B4a";
    let header = "npx mcp-remote https://example.com/mcp --header \"Authorization: Bearer opaquecredential123\"";
    let user = "https://ghp_16C7e42F292c6912E7710c838347Ae178B4a@github.com/org/private-mcp";
    let cases = [
        (
            json!({"type": "http", "url": hook}),
            "URL",
            format!(
                "[mcp_servers.github]\nurl = \"https://hooks.slack.com/services/T0123ABCD/B0456EFGH/{MASK}\"\n"
            ),
            "[mcp_servers.github]\nurl = \"https://hooks.slack.com/services/T0123ABCD/B0456EFGH\"\n",
            format!("[mcp_servers.github]\nurl = \"{hook}\"\n"),
        ),
        (
            json!({"command": command}),
            "Command",
            format!(
                "[mcp_servers.github]\ncommand = \"env  API_KEY={MASK}   node server.js --token {MASK}\"\n"
            ),
            "[mcp_servers.github]\ncommand = \"env   node server.js\"\n",
            format!("[mcp_servers.github]\ncommand = \"{command}\"\n"),
        ),
        (
            json!({"command": header}),
            "Command",
            format!(
                "[mcp_servers.github]\ncommand = \"npx mcp-remote https://example.com/mcp --header {MASK}\"\n"
            ),
            "[mcp_servers.github]\ncommand = \"npx mcp-remote https://example.com/mcp --header\"\n",
            format!("[mcp_servers.github]\ncommand = '{header}'\n"),
        ),
        (
            json!({"type": "http", "url": user}),
            "URL",
            format!("[mcp_servers.github]\nurl = \"https://{MASK}@github.com/org/private-mcp\"\n"),
            "[mcp_servers.github]\nurl = \"https://github.com/org/private-mcp\"\n",
            format!("[mcp_servers.github]\nurl = \"{user}\"\n"),
        ),
    ];
    for (server, label, masked, left_out, filled) in cases {
        claude_server(&fixture, server);
        let _ = fs::remove_file(&config);
        let item = fixture.item(&fixture.claude, "github");
        let copy = |include: &[String]| {
            preview(
                &item,
                &fixture.claude,
                fixture.to(&fixture.codex),
                &fixture.project,
                include,
            )
            .unwrap()
        };
        assert_eq!(copy(&[label.into()]).text, masked);
        let made = copy(&[]);
        assert_eq!(made.text, left_out);
        assert_eq!(made.pair.left_out, [label]);
        let pair = made.pair.clone();
        fixture.copy(&fixture.codex, made);
        assert_eq!(fs::read_to_string(&config).unwrap(), left_out);
        let drift = || drift(&pair, &fixture.claude, &fixture.codex, &fixture.project);
        assert_eq!(drift(), Some(Drift::Needs(vec![label.into()])));
        write(&config, &filled);
        assert_eq!(drift(), Some(Drift::Same), "the user filled in the {label}");
    }
}

#[test]
fn a_url_or_command_is_needed_until_every_secret_it_holds_is_back() {
    let fixture = fixture();
    let config = fixture.root.join("codex/config.toml");
    let uuid = "3f1c2b4a-9d8e-4f7a-b6c5-1a2b3c4d5e6f";
    let url = format!("https://mcp.example.com/{uuid}/mcp?api_key=q-secret-123");
    let token = "ghp_16C7e42F292c6912E7710c838347Ae178B4a";
    let command = format!("node server.js --token {token} --api-key=abcd1234");
    let cases = [
        (
            json!({"type": "http", "url": url}),
            "URL",
            format!("url = \"https://mcp.example.com/{uuid}/mcp\""),
            format!("url = \"{url}\""),
        ),
        (
            json!({"command": command}),
            "Command",
            format!("command = \"node server.js --token {token}\""),
            format!("command = \"{command}\""),
        ),
    ];
    for (server, label, one, every) in cases {
        claude_server(&fixture, server);
        let _ = fs::remove_file(&config);
        let item = fixture.item(&fixture.claude, "github");
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
        write(&config, &format!("[mcp_servers.github]\n{one}\n"));
        assert_eq!(
            drift(),
            Some(Drift::Needs(vec![label.into()])),
            "{one} still lacks a secret"
        );
        write(&config, &format!("[mcp_servers.github]\n{every}\n"));
        assert_eq!(drift(), Some(Drift::Same), "every secret is back");
    }
}

#[test]
fn a_preview_changes_with_what_its_source_gives_and_not_with_the_rest_of_its_file() {
    let fixture = fixture();
    claude_server(&fixture, github());
    let item = fixture.item(&fixture.claude, "github");
    let server = || {
        preview(
            &item,
            &fixture.claude,
            fixture.to(&fixture.codex),
            &fixture.project,
            &[],
        )
    };
    let shown = server();
    let global = fixture.root.join("claude/.claude.json");
    let mut file: Value = serde_json::from_str(&fs::read_to_string(&global).unwrap()).unwrap();
    file["numStartups"] = 7.into();
    file["mcpServers"]["other"] = json!({"command": "uvx"});
    write(&global, &file.to_string());
    assert_eq!(server(), shown, "the rest of the file is not the copy");
    let mut edited = github();
    edited["args"][1] = "server-github-v2".into();
    claude_server(&fixture, edited);
    assert_ne!(server(), shown, "its arguments changed");

    let folder = fixture.root.join("claude/skills/review");
    write(&folder.join("SKILL.md"), "---\nname: review\n---\nBody.\n");
    let item = fixture.item(&fixture.claude, "review");
    let skill = || {
        preview(
            &item,
            &fixture.claude,
            fixture.to(&fixture.codex),
            &fixture.project,
            &[],
        )
    };
    let shown = skill().unwrap();
    write(
        &folder.join("SKILL.md"),
        "---\nname: review\n---\nEdited.\n",
    );
    let edited = skill().unwrap();
    assert_eq!(edited.text, shown.text, "the same files");
    assert_ne!(edited, shown, "SKILL.md says something else");
    fs::remove_dir_all(&folder).unwrap();
    assert_eq!(
        skill().map(|preview| preview.text),
        Err("review is no longer there.".into())
    );
}
