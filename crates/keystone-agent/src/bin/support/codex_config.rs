//! The scripted Codex's config commands: `codex mcp add` and `remove`. Each writes
//! `config.toml` as Codex 0.163.0 lays it out (`docs/research/integrations-probe.md`), prints what it printed and exits as it
//! did. `KEYSTONE_FAKE_ENV_RECEIPT` records each call with its arguments.

use std::path::PathBuf;

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

/// Runs `codex <args>` when it is a config command, else returns.
pub fn codex(args: &[String]) {
    let mut args: Vec<String> = args.to_vec();
    if args.first().map(String::as_str) != Some("mcp") {
        return;
    }
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join(".codex"));
    let file = home.join("config.toml");
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .unwrap_or_else(|error: toml_edit::TomlError| fail(error.to_string()));
    let servers = doc
        .entry("mcp_servers")
        .or_insert_with(|| {
            let mut table = toml_edit::Table::new();
            table.set_implicit(true);
            toml_edit::Item::Table(table)
        })
        .as_table_mut()
        .unwrap_or_else(|| fail("mcp_servers is not a table".into()));
    match args.get(1).map(String::as_str) {
        Some("add") => {
            let command = args
                .iter()
                .position(|arg| arg == "--")
                .map(|at| args.split_off(at)[1..].to_vec())
                .unwrap_or_default();
            let url = take(&mut args, "--url");
            let bearer = take(&mut args, "--bearer-token-env-var");
            let mut env = toml_edit::Table::new();
            while let Some(pair) = take(&mut args, "--env") {
                let (key, value) = pair.split_once('=').unwrap_or((&pair, ""));
                env.insert(key, toml_edit::value(value));
            }
            let Some(name) = args.get(2).cloned() else {
                fail("error: the following required arguments were not provided: <NAME>".into())
            };
            let mut server = toml_edit::Table::new();
            match (command.split_first(), url) {
                (Some((program, rest)), None) => {
                    server.insert("command", toml_edit::value(program.as_str()));
                    let rest: toml_edit::Array = rest.iter().map(String::as_str).collect();
                    server.insert("args", toml_edit::value(rest));
                    if !env.is_empty() {
                        server.insert("env", toml_edit::Item::Table(env));
                    }
                }
                (None, Some(url)) => {
                    server.insert("url", toml_edit::value(url));
                    if let Some(bearer) = bearer {
                        server.insert("bearer_token_env_var", toml_edit::value(bearer));
                    }
                }
                _ => fail("error: exactly one of --url or -- <COMMAND> is required".into()),
            }
            servers.insert(&name, toml_edit::Item::Table(server));
            println!("Added global MCP server '{name}'.");
        }
        Some("remove") => {
            let name = args.get(2).cloned().unwrap_or_default();
            if servers.remove(&name).is_none() {
                println!("No MCP server named '{name}' found.");
                std::process::exit(0);
            }
            println!("Removed global MCP server '{name}'.");
        }
        _ => return,
    }
    let _ = std::fs::create_dir_all(&home);
    std::fs::write(&file, doc.to_string()).unwrap_or_else(|error| fail(error.to_string()));
    std::process::exit(0);
}
