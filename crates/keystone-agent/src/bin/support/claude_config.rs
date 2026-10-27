//! The scripted Claude's config commands: `claude mcp add-json` and `remove`, and
//! `claude plugin enable` and `disable`. Each writes the files the real CLI writes, as
//! Claude Code 2.1.301 lays them out (`docs/research/integrations-probe.md`), prints what it printed and exits as it
//! did. `KEYSTONE_FAKE_ENV_RECEIPT` records each call with its arguments.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};

fn fail(message: String) -> ! {
    eprintln!("{message}");
    std::process::exit(1);
}

/// The value after `flag` in `args`, and `args` without both.
fn take(args: &mut Vec<String>, flag: &str) -> Option<String> {
    let at = args.iter().position(|arg| arg == flag)?;
    args.remove(at);
    (at < args.len()).then(|| args.remove(at))
}

fn read_json(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| json!({}))
}

fn write_json(path: &Path, value: &Value) {
    if let Some(folder) = path.parent() {
        let _ = std::fs::create_dir_all(folder);
    }
    let text = serde_json::to_string_pretty(value).unwrap() + "\n";
    std::fs::write(path, text).unwrap_or_else(|error| fail(error.to_string()));
}

/// The working directory's git root, where Claude keys a project, or the directory.
fn project_key() -> String {
    let cwd = std::env::current_dir().unwrap();
    cwd.ancestors()
        .find(|folder| folder.join(".git").exists())
        .unwrap_or(&cwd)
        .display()
        .to_string()
}

fn claude_global() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir).join(".claude.json"),
        None => PathBuf::from(std::env::var_os("HOME").unwrap()).join(".claude.json"),
    }
}

fn claude_config_dir() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").unwrap()).join(".claude"),
    }
}

/// The file and JSON path where `scope`'s servers live.
fn claude_servers(scope: &str) -> (PathBuf, Vec<String>) {
    let servers = "mcpServers".to_owned();
    match scope {
        "user" => (claude_global(), vec![servers]),
        "local" => (
            claude_global(),
            vec!["projects".into(), project_key(), servers],
        ),
        "project" => (PathBuf::from(".mcp.json"), vec![servers]),
        other => fail(format!("Invalid scope: {other}")),
    }
}

fn at<'a>(value: &'a mut Value, path: &[String]) -> &'a mut Value {
    path.iter().fold(value, |value, key| {
        if !value.is_object() {
            *value = json!({});
        }
        value
            .as_object_mut()
            .unwrap()
            .entry(key.clone())
            .or_insert_with(|| json!({}))
    })
}

/// Runs `claude <args>` when it is a config command, else returns.
pub fn claude(args: &[String]) {
    let mut args: Vec<String> = args.to_vec();
    let scope = take(&mut args, "-s").or_else(|| take(&mut args, "--scope"));
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["mcp", "add-json", name, definition] => {
            let scope = scope.unwrap_or_else(|| "local".into());
            let definition: Value = serde_json::from_str(definition)
                .ok()
                .filter(|d: &Value| d["command"].is_string() || d["url"].is_string())
                .unwrap_or_else(|| fail("Invalid configuration: : Invalid input".into()));
            let (file, path) = claude_servers(&scope);
            let mut value = read_json(&file);
            let servers = at(&mut value, &path);
            if servers.get(name).is_some() {
                fail(format!(
                    "MCP server {name} already exists in {scope} config"
                ));
            }
            servers[name] = definition.clone();
            write_json(&file, &value);
            let kind = definition["type"]
                .as_str()
                .unwrap_or(if definition["url"].is_string() {
                    "http"
                } else {
                    "stdio"
                });
            println!("Added {kind} MCP server {name} to {scope} config");
        }
        ["mcp", "remove", name] => {
            let scopes = match &scope {
                Some(scope) => vec![scope.as_str()],
                None => vec!["local", "project", "user"],
            };
            for scope in scopes.iter().copied() {
                let (file, path) = claude_servers(scope);
                let mut value = read_json(&file);
                let servers = at(&mut value, &path);
                if servers.as_object_mut().unwrap().remove(name).is_some() {
                    write_json(&file, &value);
                    println!("Removed MCP server {name} from {scope} config");
                    println!("File modified: {}", file.display());
                    std::process::exit(0);
                }
            }
            fail(format!(
                "No MCP server named \"{name}\" in {} scope",
                scopes[0]
            ));
        }
        ["plugin", verb @ ("enable" | "disable"), id] => {
            let file = match scope.as_deref().unwrap_or("user") {
                "user" => claude_config_dir().join("settings.json"),
                "project" => PathBuf::from(".claude/settings.json"),
                "local" => PathBuf::from(".claude/settings.local.json"),
                other => fail(format!("Invalid scope: {other}")),
            };
            let mut value = read_json(&file);
            at(&mut value, &["enabledPlugins".into()])[id] = json!(verb == "enable");
            write_json(&file, &value);
            println!(
                "\u{2714} {}d plugin {id}",
                if verb == "enable" {
                    "Enable"
                } else {
                    "Disable"
                }
            );
        }
        _ => return,
    }
    std::process::exit(0);
}
