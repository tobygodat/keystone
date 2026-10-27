//! Changes to the MCP servers, skills and plugins one account loads: planned from an
//! [`Item`] that [`super::read`] listed, and applied only while every file the change
//! writes is still as that read found it.
//!
//! A change goes through the provider's own CLI where one covers it: `claude mcp
//! add-json` and `remove`, `claude plugin enable` and `disable`, and `codex mcp add` and
//! `remove`. Otherwise Keystone edits the file and keeps what it does not change: TOML
//! through `toml_edit`, so comments and layout stay, and JSON with every key in its
//! order. The default Claude account's `.claude.json` is never edited directly, since
//! Claude rewrites it as it runs; a change only that file could hold is refused.
//!
//! Every file a change may write is backed up first, an edited file is replaced
//! atomically, and a removed skill folder moves to the backups.

use super::{Item, Kind, Location, Scope, fingerprint_now, project_roots, same_dir};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// How an MCP server is reached.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Target {
    Command { command: String, args: Vec<String> },
    Url(String),
}

/// An MCP server's definition, as a form gives it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Server {
    pub target: Target,
    /// Each environment variable's name, and its value or `None` to keep the one it has.
    pub env: Vec<(String, Option<String>)>,
}

#[derive(Clone, Debug)]
pub enum Change {
    /// Turn an MCP server or plugin on or off.
    Switch { item: Item, on: bool },
    /// Replace an MCP server's definition.
    Edit { item: Item, server: Server },
    /// Remove an MCP server or a skill folder.
    Remove { item: Item },
    AddServer {
        scope: Scope,
        name: String,
        server: Server,
    },
    /// Copy a skill folder in.
    AddSkill { scope: Scope, folder: PathBuf },
    /// Add an MCP server as the provider's own definition: Claude's JSON, or Codex's
    /// table as JSON.
    AddDefinition {
        scope: Scope,
        name: String,
        definition: Value,
    },
}

/// The provider CLI's arguments, and the folder it runs in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cli {
    pub args: Vec<String>,
    pub cwd: PathBuf,
}

type EditText = Box<dyn FnOnce(&str) -> Result<String, String> + Send>;

enum Step {
    Cli(Cli),
    /// Replace the file with what `edit` makes of its text, empty when it is missing.
    Edit {
        path: PathBuf,
        edit: EditText,
    },
    CopyFolder {
        from: PathBuf,
        to: PathBuf,
    },
    RemoveFolder(PathBuf),
}

/// What one change does, made by [`plan`] and carried out by [`apply`].
pub struct Plan {
    /// The files whose content must still be what the read found.
    guards: Vec<PathBuf>,
    steps: Vec<Step>,
    /// What puts back the first step when a later one fails.
    undo: Vec<Step>,
    project: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Failure {
    /// The file changed since it was read. Nothing was written.
    Conflict(PathBuf),
    Failed(String),
}

const ONLY_CLAUDE: &str = "Use /mcp in Claude.";

fn claude_scope(scope: Scope) -> &'static str {
    match scope {
        Scope::User => "user",
        Scope::Local => "local",
        Scope::Project => "project",
    }
}

fn args(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

/// The steps that make `change` to the account whose files `location` names, for
/// `project`. `claude_writes_global` says Claude rewrites the account's `.claude.json` as
/// it runs: it is the default account's, or a session of the account is running. An `Err` is a change
/// Keystone will not make, saying why.
pub fn plan(
    location: &Location,
    claude_writes_global: bool,
    project: &Path,
    change: Change,
) -> Result<Plan, String> {
    let mut plan = Plan {
        guards: Vec::new(),
        steps: Vec::new(),
        undo: Vec::new(),
        project: project.to_owned(),
    };
    match (location, change) {
        (_, Change::AddSkill { scope, folder }) => {
            if !folder.join("SKILL.md").is_file() {
                return Err("That folder has no SKILL.md.".into());
            }
            let name = folder.file_name().ok_or("Choose a skill folder.")?;
            let root = skills_root(location, project, scope)?;
            plan.steps.push(Step::CopyFolder {
                to: root.join(name),
                from: folder,
            });
        }
        (_, Change::Remove { item }) if item.kind() == Kind::Skill => {
            if let super::Detail::Skill {
                plugin: Some(plugin),
                ..
            } = &item.detail
            {
                return Err(format!("This skill comes with {plugin}."));
            }
            plan.guards.push(item.source.clone());
            plan.steps.push(Step::RemoveFolder(item.source));
        }
        (
            Location::Claude {
                config_dir,
                global_file,
            },
            change,
        ) => claude(
            &mut plan,
            config_dir,
            global_file,
            claude_writes_global,
            project,
            change,
        )?,
        (Location::Codex { home, .. }, change) => codex(&mut plan, home, project, change)?,
    }
    Ok(plan)
}

pub(super) fn skills_root(
    location: &Location,
    project: &Path,
    scope: Scope,
) -> Result<PathBuf, String> {
    Ok(match (location, scope) {
        (Location::Claude { config_dir, .. }, Scope::User) => config_dir.join("skills"),
        (Location::Claude { .. }, Scope::Project) => project.join(".claude/skills"),
        (Location::Codex { home, .. }, Scope::User) => home.join("skills"),
        (Location::Codex { .. }, Scope::Project) => project.join(".agents/skills"),
        (_, Scope::Local) => return Err("Skills have no local scope.".into()),
    })
}

fn claude(
    plan: &mut Plan,
    config_dir: &Path,
    global_file: &Path,
    claude_writes_global: bool,
    project: &Path,
    change: Change,
) -> Result<(), String> {
    // A project server's file may be in a folder above the project; the CLI writes
    // the one in the folder it runs in.
    let cwd = |item: &Item| match item.scope {
        Scope::Project => item.source.parent().unwrap_or(project).to_owned(),
        _ => project.to_owned(),
    };
    let change = match change {
        Change::AddServer {
            scope,
            name,
            server,
        } => Change::AddDefinition {
            scope,
            name,
            definition: claude_json(&server, None)?,
        },
        change => change,
    };
    match change {
        Change::Switch { item, on } if item.kind() == Kind::Plugin => {
            let scope = item.scope;
            plan.guards.push(match scope {
                Scope::User => config_dir.join("settings.json"),
                Scope::Project => project.join(".claude/settings.json"),
                Scope::Local => project.join(".claude/settings.local.json"),
            });
            let verb = if on { "enable" } else { "disable" };
            plan.steps.push(Step::Cli(Cli {
                args: args(&["plugin", verb, "-s", claude_scope(scope), &item.name]),
                cwd: project.to_owned(),
            }));
        }
        Change::Switch { item, on } => {
            let global = read_json(global_file)?;
            let key = claude_project_key(&global, project);
            let in_global =
                |list: &str| super::strings(&global["projects"][&key][list]).contains(&item.name);
            let global_lists: Vec<&'static str> = match item.scope {
                Scope::Project => {
                    let local = project.join(".claude/settings.local.json");
                    plan.guards.push(local.clone());
                    let name = item.name.clone();
                    plan.steps.push(Step::Edit {
                        path: local,
                        edit: Box::new(move |text| {
                            json_edit(text, |value| {
                                listed(value, &["disabledMcpjsonServers"], &name, !on)
                            })
                        }),
                    });
                    let lists = ["disabledMcpjsonServers", "disabledMcpServers"];
                    lists
                        .into_iter()
                        .filter(|list| on && in_global(list))
                        .collect()
                }
                _ if on == in_global("disabledMcpServers") => vec!["disabledMcpServers"],
                _ => Vec::new(),
            };
            if !global_lists.is_empty() {
                if claude_writes_global {
                    return Err(ONLY_CLAUDE.into());
                }
                plan.guards.push(global_file.to_owned());
                let name = item.name.clone();
                plan.steps.push(Step::Edit {
                    path: global_file.to_owned(),
                    edit: Box::new(move |text| {
                        json_edit(text, |value| {
                            global_lists.iter().try_for_each(|list| {
                                listed(value, &["projects", &key, list], &name, !on)
                            })
                        })
                    }),
                });
            }
        }
        Change::Edit { item, server } => {
            let existing =
                claude_server(global_file, project, &item.source, item.scope, &item.name)
                    .ok_or_else(|| format!("{} is no longer there.", item.name))?;
            let new = claude_json(&server, Some(&existing))?;
            let scope = claude_scope(item.scope);
            let (name, cwd) = (item.name.clone(), cwd(&item));
            plan.guards.push(item.source);
            plan.steps.push(Step::Cli(Cli {
                args: args(&["mcp", "remove", "-s", scope, &name]),
                cwd: cwd.clone(),
            }));
            for (steps, definition) in [(&mut plan.steps, new), (&mut plan.undo, existing)] {
                steps.push(Step::Cli(Cli {
                    args: args(&[
                        "mcp",
                        "add-json",
                        "-s",
                        scope,
                        &name,
                        &canonical(&definition),
                    ]),
                    cwd: cwd.clone(),
                }));
            }
        }
        Change::Remove { item } => {
            let cwd = cwd(&item);
            let remove = args(&["mcp", "remove", "-s", claude_scope(item.scope), &item.name]);
            plan.guards.push(item.source);
            plan.steps.push(Step::Cli(Cli { args: remove, cwd }));
        }
        Change::AddDefinition {
            scope,
            name,
            definition,
        } => {
            let definition = canonical(&definition);
            plan.guards.push(match scope {
                Scope::Project => project.join(".mcp.json"),
                _ => global_file.to_owned(),
            });
            plan.steps.push(Step::Cli(Cli {
                args: args(&[
                    "mcp",
                    "add-json",
                    "-s",
                    claude_scope(scope),
                    &name,
                    &definition,
                ]),
                cwd: project.to_owned(),
            }));
        }
        Change::AddServer { .. } => unreachable!("made an AddDefinition above"),
        Change::AddSkill { .. } => unreachable!("planned for every provider"),
    }
    Ok(())
}

fn codex(plan: &mut Plan, home: &Path, project: &Path, change: Change) -> Result<(), String> {
    let config = home.join("config.toml");
    match change {
        Change::Switch { item, on } => {
            let name = item.name.clone();
            let plugin = item.kind() == Kind::Plugin;
            plan.guards.push(item.source.clone());
            plan.steps.push(Step::Edit {
                path: item.source,
                edit: Box::new(move |text| match plugin {
                    true => toml_switch_plugin(text, &name, on),
                    false => toml_switch_server(text, &name, on),
                }),
            });
        }
        Change::Edit { item, server } => {
            let name = item.name.clone();
            plan.guards.push(item.source.clone());
            plan.steps.push(Step::Edit {
                path: item.source,
                edit: Box::new(move |text| toml_put_server(text, &name, &server, false)),
            });
        }
        Change::Remove { item } if item.scope == Scope::User => {
            plan.guards.push(item.source);
            plan.steps.push(Step::Cli(Cli {
                args: args(&["mcp", "remove", &item.name]),
                cwd: project.to_owned(),
            }));
        }
        Change::Remove { item } => {
            let name = item.name.clone();
            plan.guards.push(item.source.clone());
            plan.steps.push(Step::Edit {
                path: item.source,
                edit: Box::new(move |text| toml_remove_server(text, &name)),
            });
        }
        Change::AddServer {
            scope: Scope::User,
            name,
            server,
        } => {
            let text = fs::read_to_string(&config).unwrap_or_default();
            let existing: toml::Table =
                toml::from_str(&text).map_err(|error| error.message().to_owned())?;
            if existing
                .get("mcp_servers")
                .and_then(|servers| servers.get(&name))
                .is_some()
            {
                return Err(taken(&name));
            }
            let mut cli = args(&["mcp", "add", &name]);
            match server.target {
                Target::Url(url) if server.env.is_empty() => cli.extend(["--url".into(), url]),
                Target::Url(_) => return Err(NO_URL_ENV.into()),
                Target::Command { command, args } => {
                    for (variable, value) in server.env {
                        let value = value.ok_or_else(|| needs_value(&variable))?;
                        cli.extend(["--env".into(), format!("{variable}={value}")]);
                    }
                    cli.push("--".into());
                    cli.push(command);
                    cli.extend(args);
                }
            }
            plan.guards.push(config);
            plan.steps.push(Step::Cli(Cli {
                args: cli,
                cwd: project.to_owned(),
            }));
        }
        Change::AddServer {
            scope: Scope::Project,
            name,
            server,
        } => {
            let path = project.join(".codex/config.toml");
            plan.guards.push(path.clone());
            plan.steps.push(Step::Edit {
                path,
                edit: Box::new(move |text| toml_put_server(text, &name, &server, true)),
            });
        }
        Change::AddServer { .. } => return Err(NO_LOCAL.into()),
        Change::AddDefinition {
            scope,
            name,
            definition,
        } => {
            let path = match scope {
                Scope::User => config,
                Scope::Project => project.join(".codex/config.toml"),
                Scope::Local => return Err(NO_LOCAL.into()),
            };
            let table = codex_text(&name, &definition)?;
            plan.guards.push(path.clone());
            plan.steps.push(Step::Edit {
                path,
                edit: Box::new(move |text| toml_append(text, &name, &table)),
            });
        }
        Change::AddSkill { .. } => unreachable!("planned for every provider"),
    }
    Ok(())
}

pub(super) const NO_LOCAL: &str = "Codex has no local scope.";

fn taken(name: &str) -> String {
    format!("A server named {name} is already there.")
}

const NO_URL_ENV: &str = "Environment variables need a command, not a URL.";

fn needs_value(variable: &str) -> String {
    format!("Give {variable} a value.")
}

fn read_json(path: &Path) -> Result<Value, String> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|error| error.to_string()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Value::Null),
        Err(error) => Err(error.to_string()),
    }
}

/// The key of `project`'s entry in `.claude.json`'s `projects`: an existing entry for
/// the project or a folder above it up to the git root, else the git root, where
/// Claude keys a new one.
fn claude_project_key(global: &Value, project: &Path) -> String {
    let roots = project_roots(project);
    roots
        .iter()
        .rev()
        .find_map(|root| {
            global["projects"]
                .as_object()?
                .keys()
                .find(|key| same_dir(Path::new(key), root))
                .cloned()
        })
        .unwrap_or_else(|| {
            roots
                .last()
                .unwrap_or(&project.to_owned())
                .display()
                .to_string()
        })
}

/// The server `name` at `scope` in `source` as Claude keeps it, secrets included.
pub(super) fn claude_server(
    global_file: &Path,
    project: &Path,
    source: &Path,
    scope: Scope,
    name: &str,
) -> Option<Value> {
    let file = read_json(source).ok()?;
    let server = match scope {
        Scope::User | Scope::Project => &file["mcpServers"][name],
        Scope::Local => {
            let key = claude_project_key(&read_json(global_file).ok()?, project);
            &file["projects"][&key]["mcpServers"][name]
        }
    };
    server.is_object().then(|| server.clone())
}

/// The server `name` in the Codex config `source`, secrets included, as JSON.
pub(super) fn codex_server(source: &Path, name: &str) -> Option<Value> {
    let config: toml::Table = toml::from_str(&fs::read_to_string(source).ok()?).ok()?;
    let server = config.get("mcp_servers")?.get(name)?.as_table()?;
    serde_json::to_value(server).ok()
}

/// `item`'s definition for a form, its environment variables named but not given.
/// `None` when the file no longer has it or it is not an MCP server.
pub fn server(location: &Location, project: &Path, item: &Item) -> Option<Server> {
    let env = |names: Vec<String>| names.into_iter().map(|name| (name, None)).collect();
    match location {
        Location::Claude { global_file, .. } => {
            let server = claude_server(global_file, project, &item.source, item.scope, &item.name)?;
            let target = match server["url"].as_str() {
                Some(url) if server["command"].is_null() => Target::Url(url.to_owned()),
                _ => Target::Command {
                    command: server["command"].as_str()?.to_owned(),
                    args: super::strings(&server["args"]),
                },
            };
            Some(Server {
                target,
                env: env({
                    let mut names = super::keys(&server["env"]);
                    names.sort();
                    names
                }),
            })
        }
        Location::Codex { .. } => {
            let text = fs::read_to_string(&item.source).ok()?;
            let config: toml::Table = toml::from_str(&text).ok()?;
            let server = config.get("mcp_servers")?.get(&item.name)?.as_table()?;
            let text = |key: &str| server.get(key).and_then(toml::Value::as_str);
            let target = match (text("command"), text("url")) {
                (Some(command), _) => Target::Command {
                    command: command.to_owned(),
                    args: server
                        .get("args")
                        .and_then(toml::Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|arg| arg.as_str().map(str::to_owned))
                        .collect(),
                },
                (None, Some(url)) => Target::Url(url.to_owned()),
                (None, None) => return None,
            };
            let names = server
                .get("env")
                .and_then(toml::Value::as_table)
                .map(|env| env.keys().cloned().collect())
                .unwrap_or_default();
            Some(Server {
                target,
                env: env(names),
            })
        }
    }
}

/// `server` as `claude mcp add-json` takes it, over the `existing` definition so the
/// keys a form does not show stay.
fn claude_json(server: &Server, existing: Option<&Value>) -> Result<Value, String> {
    let mut definition = existing
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let old_env = definition.remove("env").unwrap_or_default();
    let mut env = serde_json::Map::new();
    for (name, value) in &server.env {
        let value = match value {
            Some(value) => Value::String(value.clone()),
            None => old_env
                .get(name)
                .cloned()
                .ok_or_else(|| needs_value(name))?,
        };
        env.insert(name.clone(), value);
    }
    match &server.target {
        Target::Command { command, args } => {
            for key in ["url", "headers"] {
                definition.remove(key);
            }
            definition.insert("type".into(), "stdio".into());
            definition.insert("command".into(), command.clone().into());
            definition.insert("args".into(), args.clone().into());
            if !env.is_empty() {
                definition.insert("env".into(), env.into());
            }
        }
        Target::Url(_) if !env.is_empty() => return Err(NO_URL_ENV.into()),
        Target::Url(url) => {
            for key in ["command", "args"] {
                definition.remove(key);
            }
            let remote = matches!(definition.get("type").and_then(Value::as_str), Some(kind) if kind != "stdio");
            if !remote {
                definition.insert("type".into(), "http".into());
            }
            definition.insert("url".into(), url.clone().into());
        }
    }
    Ok(definition.into())
}

/// JSON that keeps its objects' keys in the file's order, whatever features
/// `serde_json` is built with.
#[derive(Clone, Debug, PartialEq)]
enum Json {
    Leaf(Value),
    List(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl<'de> serde::Deserialize<'de> for Json {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Json;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("JSON")
            }
            fn visit_bool<E>(self, v: bool) -> Result<Json, E> {
                Ok(Json::Leaf(v.into()))
            }
            fn visit_i64<E>(self, v: i64) -> Result<Json, E> {
                Ok(Json::Leaf(v.into()))
            }
            fn visit_u64<E>(self, v: u64) -> Result<Json, E> {
                Ok(Json::Leaf(v.into()))
            }
            fn visit_f64<E>(self, v: f64) -> Result<Json, E> {
                Ok(Json::Leaf(v.into()))
            }
            fn visit_str<E>(self, v: &str) -> Result<Json, E> {
                Ok(Json::Leaf(v.into()))
            }
            fn visit_unit<E>(self) -> Result<Json, E> {
                Ok(Json::Leaf(Value::Null))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
                let mut list = Vec::new();
                while let Some(item) = seq.next_element()? {
                    list.push(item);
                }
                Ok(Json::List(list))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
                let mut object: Vec<(String, Json)> = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, Json>()? {
                    match object.iter_mut().find(|(k, _)| *k == key) {
                        Some(entry) => entry.1 = value,
                        None => object.push((key, value)),
                    }
                }
                Ok(Json::Object(object))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

impl serde::Serialize for Json {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap, SerializeSeq};
        match self {
            Json::Leaf(value) => value.serialize(serializer),
            Json::List(list) => {
                let mut seq = serializer.serialize_seq(Some(list.len()))?;
                for item in list {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Json::Object(object) => {
                let mut map = serializer.serialize_map(Some(object.len()))?;
                for (key, value) in object {
                    map.serialize_entry(key, value)?;
                }
                map.end()
            }
        }
    }
}

/// `value` as compact JSON with every object's keys sorted, the same whatever features
/// `serde_json` is built with.
pub(super) fn canonical(value: &Value) -> String {
    fn sorted(value: &Value) -> Json {
        match value {
            Value::Array(list) => Json::List(list.iter().map(sorted).collect()),
            Value::Object(object) => {
                let mut pairs: Vec<(String, Json)> =
                    object.iter().map(|(k, v)| (k.clone(), sorted(v))).collect();
                pairs.sort_by(|a, b| a.0.cmp(&b.0));
                Json::Object(pairs)
            }
            leaf => Json::Leaf(leaf.clone()),
        }
    }
    serde_json::to_string(&sorted(value)).unwrap_or_default()
}

/// `text` as JSON after `edit`, two-space indented as Claude writes it, every other
/// key kept in its place. Unchanged text comes back as it was.
fn json_edit(
    text: &str,
    edit: impl FnOnce(&mut Json) -> Result<(), String>,
) -> Result<String, String> {
    let mut value: Json = match text.trim() {
        "" => Json::Object(Vec::new()),
        text => serde_json::from_str(text).map_err(|error| error.to_string())?,
    };
    let before = value.clone();
    edit(&mut value)?;
    if value == before {
        return Ok(text.to_owned());
    }
    let mut out = serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?;
    if text.is_empty() || text.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Puts `name` in, or takes it out of, the list at `path` under `value`, making the
/// objects on the way.
fn listed(value: &mut Json, path: &[&str], name: &str, listed: bool) -> Result<(), String> {
    let mut at = value;
    for key in path {
        let Json::Object(object) = at else {
            return Err(format!("{key} is not inside an object"));
        };
        let index = match object.iter().position(|(k, _)| k == key) {
            Some(index) => index,
            None if !listed => return Ok(()),
            None => {
                object.push(((*key).to_owned(), Json::Object(Vec::new())));
                object.len() - 1
            }
        };
        at = &mut object[index].1;
    }
    if *at == Json::Leaf(Value::Null) || *at == Json::Object(Vec::new()) {
        if !listed {
            return Ok(());
        }
        *at = Json::List(Vec::new());
    }
    let Json::List(list) = at else {
        return Err("expected a list".into());
    };
    let item = Json::Leaf(name.into());
    let there = list.contains(&item);
    if listed && !there {
        list.push(item);
    } else if !listed {
        list.retain(|v| *v != item);
    }
    Ok(())
}

fn toml_doc(text: &str) -> Result<toml_edit::DocumentMut, String> {
    text.parse()
        .map_err(|error: toml_edit::TomlError| error.message().to_owned())
}

fn toml_table<'a>(
    doc: &'a mut toml_edit::DocumentMut,
    group: &str,
    name: &str,
) -> Result<&'a mut dyn toml_edit::TableLike, String> {
    doc.get_mut(group)
        .and_then(toml_edit::Item::as_table_like_mut)
        .and_then(|group| group.get_mut(name))
        .and_then(toml_edit::Item::as_table_like_mut)
        .ok_or_else(|| format!("{name} is no longer there."))
}

/// Codex's `enabled = false` on the server `name`, or its removal to turn it on. The
/// rest of the file, comments included, is untouched.
fn toml_switch_server(text: &str, name: &str, on: bool) -> Result<String, String> {
    let mut doc = toml_doc(text)?;
    let server = toml_table(&mut doc, "mcp_servers", name)?;
    if on {
        server.remove("enabled");
    } else {
        server.insert("enabled", toml_edit::value(false));
    }
    Ok(doc.to_string())
}

fn toml_switch_plugin(text: &str, id: &str, on: bool) -> Result<String, String> {
    let mut doc = toml_doc(text)?;
    toml_table(&mut doc, "plugins", id)?.insert("enabled", toml_edit::value(on));
    Ok(doc.to_string())
}

fn toml_remove_server(text: &str, name: &str) -> Result<String, String> {
    let mut doc = toml_doc(text)?;
    doc.get_mut("mcp_servers")
        .and_then(toml_edit::Item::as_table_like_mut)
        .and_then(|servers| servers.remove(name))
        .ok_or_else(|| format!("{name} is no longer there."))?;
    Ok(doc.to_string())
}

/// Writes `server` as `[mcp_servers.<name>]`: a new table after the rest of the file
/// when `adding`, else over the one there, whose other keys and comments stay.
fn toml_put_server(
    text: &str,
    name: &str,
    server: &Server,
    adding: bool,
) -> Result<String, String> {
    if adding {
        let table = toml_server(name, |table| put_server(table, server, &BTreeMap::new()))?;
        return toml_append(text, name, &table);
    }
    let mut doc = toml_doc(text)?;
    let table = toml_table(&mut doc, "mcp_servers", name)?;
    let old_env: BTreeMap<String, toml_edit::Item> = table
        .get("env")
        .and_then(toml_edit::Item::as_table_like)
        .map(|env| env.iter().map(|(k, v)| (k.to_owned(), v.clone())).collect())
        .unwrap_or_default();
    put_server(table, server, &old_env)?;
    Ok(doc.to_string())
}

/// `[mcp_servers.<name>]` as `fill` makes it, alone.
fn toml_server(
    name: &str,
    fill: impl FnOnce(&mut dyn toml_edit::TableLike) -> Result<(), String>,
) -> Result<String, String> {
    let mut new = toml_edit::DocumentMut::new();
    let mut servers = toml_edit::Table::new();
    servers.set_implicit(true);
    servers.insert(name, toml_edit::Item::Table(toml_edit::Table::new()));
    new.insert("mcp_servers", toml_edit::Item::Table(servers));
    fill(toml_table(&mut new, "mcp_servers", name)?)?;
    Ok(new.to_string())
}

/// `text` with `table`, the server `name` alone, after the rest of it.
fn toml_append(text: &str, name: &str, table: &str) -> Result<String, String> {
    let taken = toml_doc(text)?
        .get("mcp_servers")
        .and_then(toml_edit::Item::as_table_like)
        .is_some_and(|servers| servers.contains_key(name));
    if taken {
        return Err(self::taken(name));
    }
    let gap = match text {
        "" => "",
        text if text.ends_with("\n\n") => "",
        text if text.ends_with('\n') => "\n",
        _ => "\n\n",
    };
    let joined = format!("{text}{gap}{table}");
    toml_doc(&joined)?;
    Ok(joined)
}

/// The server `name` as the text a copy appends to a Codex config: `definition`'s
/// objects become tables and its lists arrays.
pub(super) fn codex_text(name: &str, definition: &Value) -> Result<String, String> {
    fn value(json: &Value) -> Option<toml_edit::Value> {
        Some(match json {
            Value::String(text) => text.as_str().into(),
            Value::Bool(on) => (*on).into(),
            Value::Number(number) => match number.as_i64() {
                Some(whole) => whole.into(),
                None => number.as_f64()?.into(),
            },
            Value::Array(list) => list
                .iter()
                .filter_map(value)
                .collect::<toml_edit::Array>()
                .into(),
            Value::Object(object) => {
                let mut table = toml_edit::InlineTable::new();
                for (key, json) in object {
                    table.extend(value(json).map(|v| (key.as_str(), v)));
                }
                table.into()
            }
            Value::Null => return None,
        })
    }
    let object: BTreeMap<_, _> = definition
        .as_object()
        .ok_or("not a server")?
        .iter()
        .collect();
    toml_server(name, |server| {
        for (key, json) in object {
            let item = match json {
                Value::Object(inner) => {
                    let mut table = toml_edit::Table::new();
                    for (key, json) in inner.iter().collect::<BTreeMap<_, _>>() {
                        table.extend(value(json).map(|v| (key.as_str(), v)));
                    }
                    toml_edit::Item::Table(table)
                }
                json => match value(json) {
                    Some(v) => toml_edit::Item::Value(v),
                    None => continue,
                },
            };
            server.insert(key, item);
        }
        Ok(())
    })
}

fn put_server(
    table: &mut dyn toml_edit::TableLike,
    server: &Server,
    old_env: &BTreeMap<String, toml_edit::Item>,
) -> Result<(), String> {
    match &server.target {
        Target::Command { command, args } => {
            table.remove("url");
            table.remove("bearer_token_env_var");
            set(table, "command", command.as_str().into());
            if args.is_empty() {
                table.remove("args");
            } else {
                let args: toml_edit::Array = args.iter().map(String::as_str).collect();
                set(table, "args", args.into());
            }
        }
        Target::Url(_) if !server.env.is_empty() => return Err(NO_URL_ENV.into()),
        Target::Url(url) => {
            table.remove("command");
            table.remove("args");
            set(table, "url", url.as_str().into());
        }
    }
    if server.env.is_empty() {
        table.remove("env");
        return Ok(());
    }
    let mut values = Vec::new();
    for (variable, value) in &server.env {
        let value = match value {
            Some(value) => value.as_str().into(),
            None => old_env
                .get(variable)
                .and_then(toml_edit::Item::as_value)
                .cloned()
                .ok_or_else(|| needs_value(variable))?,
        };
        values.push((variable, value));
    }
    let env = table
        .entry("env")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_like_mut()
        .ok_or("env is not a table")?;
    let names: Vec<String> = env.iter().map(|(k, _)| k.to_owned()).collect();
    for old in names {
        if !server.env.iter().any(|(variable, _)| *variable == old) {
            env.remove(&old);
        }
    }
    for (variable, value) in values {
        set(env, variable, value);
    }
    Ok(())
}

/// Sets `key` to `value`, keeping the comments and spacing around the old value.
fn set(table: &mut dyn toml_edit::TableLike, key: &str, mut value: toml_edit::Value) {
    match table.get_mut(key) {
        Some(toml_edit::Item::Value(old)) => {
            *value.decor_mut() = old.decor().clone();
            *old = value;
        }
        _ => {
            table.insert(key, toml_edit::Item::Value(value));
        }
    }
}

/// Carries out `plan`: refuses with [`Failure::Conflict`] when a file it guards is not
/// what `shown` says the read found, else backs up each guarded file into `backups`
/// and runs each step, `cli` running the provider's CLI. When a step after the first
/// fails, the plan's undo runs.
pub fn apply(
    plan: Plan,
    shown: &BTreeMap<PathBuf, super::Fingerprint>,
    backups: &Path,
    cli: &mut dyn FnMut(&Cli) -> Result<(), String>,
) -> Result<(), Failure> {
    let failed = |error: io::Error| Failure::Failed(error.to_string());
    for path in &plan.guards {
        let now = fingerprint_now(path, &plan.project).map_err(failed)?;
        if now != shown.get(path).copied().flatten() {
            return Err(Failure::Conflict(path.clone()));
        }
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis());
    static APPLIED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = APPLIED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let backups = backups.join(format!("{stamp}-{}-{count}", std::process::id()));
    for path in &plan.guards {
        if path.is_file() {
            let to = backups.join(relative(path));
            private_dirs(to.parent().unwrap_or(&backups)).map_err(failed)?;
            fs::copy(path, &to).map_err(failed)?;
        }
    }
    let cli = &mut |call: &Cli| cli(call).map_err(|error| scrub(&error, call));
    let mut edited = Vec::new();
    for (at, step) in plan.steps.into_iter().enumerate() {
        let path = match &step {
            Step::Edit { path, .. } => Some((path.clone(), path.exists())),
            _ => None,
        };
        let Err(error) = run(step, (shown, &plan.project), &backups, cli) else {
            edited.extend(path);
            continue;
        };
        if at == 0 {
            return Err(error);
        }
        let error = match error {
            Failure::Conflict(path) => format!("{} changed while saving.", path.display()),
            Failure::Failed(error) => error,
        };
        let put_back = |(path, existed): &(PathBuf, bool)| {
            if *existed {
                fs::read_to_string(backups.join(relative(path))).and_then(|old| replace(path, &old))
            } else {
                fs::remove_file(path)
            }
        };
        for edit in edited.iter().rev() {
            if let Err(undo) = put_back(edit) {
                return Err(Failure::Failed(format!(
                    "{error} Putting it back failed too: {undo} The old file is in {}.",
                    backups.display()
                )));
            }
        }
        for step in plan.undo {
            if let Err(undo) = run(step, (&BTreeMap::new(), &plan.project), &backups, cli) {
                let undo = match undo {
                    Failure::Conflict(path) => path.display().to_string(),
                    Failure::Failed(undo) => undo,
                };
                return Err(Failure::Failed(format!(
                    "{error} Putting it back failed too: {undo} The old file is in {}.",
                    backups.display()
                )));
            }
        }
        return Err(Failure::Failed(error));
    }
    Ok(())
}

/// `error` with every value `call` passed that can hold a secret (an environment or
/// header value, a URL, an argument) hidden, since a CLI can echo what it was given.
fn scrub(error: &str, call: &Cli) -> String {
    let mut secrets = Vec::new();
    let mut words = call.args.iter().peekable();
    while let Some(word) = words.next() {
        if let Ok(Value::Object(definition)) = serde_json::from_str::<Value>(word) {
            for key in ["env", "headers"] {
                if let Some(Value::Object(values)) = definition.get(key) {
                    secrets.extend(values.values().filter_map(Value::as_str).map(str::to_owned));
                }
            }
            if let Some(Value::Array(args)) = definition.get("args") {
                secrets.extend(args.iter().filter_map(Value::as_str).map(str::to_owned));
            }
            secrets.extend(
                definition
                    .get("url")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            );
        }
        match word.as_str() {
            "--env" => secrets.extend(
                words
                    .next()
                    .and_then(|pair| pair.split_once('='))
                    .map(|(_, value)| value.to_owned()),
            ),
            "--url" => secrets.extend(words.next().cloned()),
            "--" => {
                words.next();
                secrets.extend(words.by_ref().cloned());
            }
            _ => {}
        }
    }
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    secrets
        .iter()
        .filter(|secret| secret.len() >= 3)
        .fold(error.to_owned(), |error, secret| {
            error.replace(secret.as_str(), "\u{2022}\u{2022}\u{2022}")
        })
}

/// Creates `dir` and the folders above it that are missing, readable by the user
/// alone: a backup holds the same secrets as the file it copies.
fn private_dirs(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

/// `path` without its root, to file it under the backups folder.
fn relative(path: &Path) -> PathBuf {
    path.components()
        .filter(|part| matches!(part, std::path::Component::Normal(_)))
        .collect()
}

/// Runs one step. An edit of a file in `shown` first checks the text it edits is still
/// what was shown, so a save from elsewhere since the first check is not overwritten.
fn run(
    step: Step,
    shown: (&BTreeMap<PathBuf, super::Fingerprint>, &Path),
    backups: &Path,
    cli: &mut dyn FnMut(&Cli) -> Result<(), String>,
) -> Result<(), Failure> {
    run_step(step, shown, backups, cli).map_err(|error| match error {
        StepError::Conflict(path) => Failure::Conflict(path),
        StepError::Failed(error) => Failure::Failed(error),
    })
}

enum StepError {
    Conflict(PathBuf),
    Failed(String),
}

impl From<String> for StepError {
    fn from(error: String) -> Self {
        StepError::Failed(error)
    }
}

impl From<&str> for StepError {
    fn from(error: &str) -> Self {
        StepError::Failed(error.to_owned())
    }
}

fn run_step(
    step: Step,
    (shown, project): (&BTreeMap<PathBuf, super::Fingerprint>, &Path),
    backups: &Path,
    cli: &mut dyn FnMut(&Cli) -> Result<(), String>,
) -> Result<(), StepError> {
    let text = |error: io::Error| StepError::Failed(error.to_string());
    match step {
        Step::Cli(call) => Ok(cli(&call)?),
        Step::Edit { path, edit } => {
            let old = match fs::read_to_string(&path) {
                Ok(old) => Some(old),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(text(error)),
            };
            if let Some(print) = shown.get(&path)
                && super::fingerprint(&path, old.as_deref(), project) != *print
            {
                return Err(StepError::Conflict(path));
            }
            let old = old.unwrap_or_default();
            let new = edit(&old)?;
            if new == old {
                return Ok(());
            }
            replace(&path, &new).map_err(text)
        }
        Step::CopyFolder { from, to } => {
            if to.exists() {
                let name = to.file_name().unwrap_or_default().to_string_lossy();
                return Err(format!("A skill named {name} is already there.").into());
            }
            let parent = to.parent().ok_or("no folder to copy into")?;
            fs::create_dir_all(parent).map_err(text)?;
            let staged = staged(&to);
            let copied = copy_folder(&from, &staged).and_then(|()| fs::rename(&staged, &to));
            if copied.is_err() {
                let _ = fs::remove_dir_all(&staged);
            }
            copied.map_err(text)
        }
        Step::RemoveFolder(folder) => {
            let to = backups.join(relative(&folder));
            private_dirs(to.parent().unwrap_or(backups)).map_err(text)?;
            fs::rename(&folder, &to)
                .or_else(|_| copy_folder(&folder, &to).and_then(|()| fs::remove_dir_all(&folder)))
                .map_err(|error| {
                    StepError::Failed(format!(
                        "{error} The old folder is in {}.",
                        backups.display()
                    ))
                })
        }
    }
}

fn copy_folder(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_folder(&entry.path(), &target)?;
        } else if kind.is_symlink() && cfg!(unix) {
            // A link stays a link: following one can copy a secret it points at.
            #[cfg(unix)]
            std::os::unix::fs::symlink(fs::read_link(entry.path())?, &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// A name beside `path` that no other write, in this process or another, is using.
fn staged(path: &Path) -> PathBuf {
    static STAGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = STAGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    path.with_file_name(format!(
        ".{}.keystone-{}-{count}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ))
}

/// Writes `text` beside `path` and renames it over `path`, keeping the file's
/// permissions, so a reader sees the old file or the new one and never part of one.
fn replace(path: &Path, text: &str) -> io::Result<()> {
    // A symlinked config, as dotfile setups keep them, stays a link to the new text.
    let resolved = fs::canonicalize(path).ok();
    let path = resolved.as_deref().unwrap_or(path);
    let folder = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(folder)?;
    let staged = staged(path);
    let written = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&staged)?;
        io::Write::write_all(&mut file, text.as_bytes())?;
        if let Ok(meta) = fs::metadata(path) {
            file.set_permissions(meta.permissions())?;
        }
        file.sync_all()?;
        fs::rename(&staged, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&staged);
    }
    written
}

#[cfg(test)]
#[path = "edit_tests.rs"]
mod tests;
