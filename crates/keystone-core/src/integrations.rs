//! The MCP servers, skills and plugins a provider account loads for a project, read
//! from the provider's own files. Nothing here writes, and nothing here decides where
//! an account's files are: the caller names them with a [`Location`].
//!
//! Secret values never leave this module. An MCP server keeps its command's first word
//! but only the number of its other words and arguments, the names of its environment variables and headers,
//! and its URL's scheme, host and port.
//!
//! Each [`Item`] names one declaration: the provider, account, scope and file it came
//! from, and its name within that file. The same server declared at two scopes is two
//! items, as each provider's own `mcp`/`plugin` commands treat them, and the one the
//! provider does not use names the scope that wins over it. The file layouts were
//! observed on Claude Code 2.1.301 and Codex 0.163.0; see
//! `docs/research/integrations-probe.md`. [`edit`] changes them.

pub mod edit;

use crate::{AccountId, Provider};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// What kind of thing an item is.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Kind {
    McpServer,
    Skill,
    Plugin,
}

/// Where a declaration applies, in the providers' own terms.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Scope {
    /// Every project the account opens.
    User,
    /// One project, kept in the account's own files: Claude's `local` scope.
    Local,
    /// One project, kept in the project's own files.
    Project,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Scope::User => "User",
            Scope::Local => "Local",
            Scope::Project => "Project",
        }
    }
}

/// What an item's own declaration says.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum State {
    On,
    /// Turned off in the provider's config.
    Off,
    /// In a project config Codex ignores until the project is trusted.
    Untrusted,
}

/// How an MCP server is reached.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Transport {
    Stdio {
        command: String,
        /// How many arguments it is given, counting any words after the first in
        /// `command`. Their text stays in the file: any of them can be a token.
        args: usize,
    },
    /// `kind` is the transport as the config names it, such as `http` or `sse`.
    Remote { kind: String, url: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpServer {
    pub transport: Transport,
    /// The names of the environment variables it is given, never their values.
    pub env: Vec<String>,
    /// The names of the HTTP headers it sends, never their values.
    pub headers: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Detail {
    McpServer(McpServer),
    Skill {
        description: String,
        /// The plugin, as `name@marketplace`, that brings this skill and owns its
        /// folder.
        plugin: Option<String>,
    },
    Plugin {
        version: Option<String>,
    },
}

/// One MCP server, skill or plugin, as one file declares it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Item {
    pub provider: Provider,
    pub account: AccountId,
    /// The name the provider knows it by: a server's key, a skill's name, or a
    /// plugin's `name@marketplace`.
    pub name: String,
    pub scope: Scope,
    /// The file that declares it, or a skill's folder.
    pub source: PathBuf,
    pub state: State,
    /// The scope of a declaration of the same name that the provider uses instead of
    /// this one.
    pub overridden_by: Option<Scope>,
    pub detail: Detail,
}

impl Item {
    pub fn kind(&self) -> Kind {
        match self.detail {
            Detail::McpServer(_) => Kind::McpServer,
            Detail::Skill { .. } => Kind::Skill,
            Detail::Plugin { .. } => Kind::Plugin,
        }
    }
}

/// A file that exists but could not be read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Problem {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Inventory {
    pub items: Vec<Item>,
    pub problems: Vec<Problem>,
    /// Every file the read looked at, missing ones included, as it found them. A
    /// change compares each file it writes with this first.
    pub files: BTreeMap<PathBuf, Fingerprint>,
}

/// A file's content as a hash, or `None` when it is missing. Of a `.claude.json`,
/// which Claude rewrites as it runs, only the parts this module reads count.
pub type Fingerprint = Option<[u8; 32]>;

/// `text` as `path`'s [`Fingerprint`] for `project`. Of a `.claude.json`, only the
/// user servers and the entries for `project` and the folders above it up to its git
/// root count: Claude rewrites the rest as it runs, in any project.
pub fn fingerprint(path: &Path, text: Option<&str>, project: &Path) -> Fingerprint {
    let text = text?;
    let hashed = match serde_json::from_str::<Value>(text) {
        Ok(global) if path.file_name().is_some_and(|name| name == ".claude.json") => {
            let roots = project_roots(project);
            let projects: serde_json::Map<String, Value> = global["projects"]
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(key, _)| roots.iter().any(|root| same_dir(Path::new(key), root)))
                .map(|(key, entry)| {
                    let kept = ["mcpServers", "disabledMcpServers", "disabledMcpjsonServers"]
                        .map(|name| (name.to_owned(), entry[name].clone()));
                    (key.clone(), Value::Object(kept.into_iter().collect()))
                })
                .collect();
            edit::canonical(&serde_json::json!([global["mcpServers"], projects]))
        }
        _ => text.to_owned(),
    };
    Some(Sha256::digest(hashed.as_bytes()).into())
}

/// `path`'s [`Fingerprint`] now, or the error that kept it from being read. A folder's
/// covers the name and content of every file in it.
pub fn fingerprint_now(path: &Path, project: &Path) -> io::Result<Fingerprint> {
    if path.is_dir() {
        let mut hasher = Sha256::new();
        folder_hash(path, Path::new(""), &mut hasher)?;
        return Ok(Some(hasher.finalize().into()));
    }
    match fs::read_to_string(path) {
        Ok(text) => Ok(fingerprint(path, Some(&text), project)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Where one account's provider keeps its files.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Location {
    Claude {
        /// `CLAUDE_CONFIG_DIR`, or `~/.claude`.
        config_dir: PathBuf,
        /// The account's `.claude.json`: in `config_dir` when `CLAUDE_CONFIG_DIR` is
        /// set, else `~/.claude.json`.
        global_file: PathBuf,
    },
    Codex {
        /// `CODEX_HOME`, or `~/.codex`.
        home: PathBuf,
        /// The user's home, whose `.agents/skills` every Codex account reads.
        user_home: Option<PathBuf>,
    },
}

impl Location {
    pub fn provider(&self) -> Provider {
        match self {
            Location::Claude { .. } => Provider::Claude,
            Location::Codex { .. } => Provider::Codex,
        }
    }
}

/// A server's connection as a running provider reported it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Health {
    Connected,
    Pending,
    NeedsAuth,
    Failed,
    Disabled,
}

impl Health {
    pub fn label(self) -> &'static str {
        match self {
            Health::Connected => "Connected",
            Health::Pending => "Starting",
            Health::NeedsAuth => "Needs sign-in",
            Health::Failed => "Failed",
            Health::Disabled => "Off",
        }
    }
}

/// One MCP server's status from a running session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerStatus {
    pub name: String,
    pub health: Health,
}

/// Everything `account` loads for `project` from the files `location` names. Missing
/// files are normal and give nothing; a file that exists but cannot be read or parsed
/// gives a [`Problem`] and the rest is still read. Items are sorted by kind, then name.
pub fn read(account: &AccountId, location: &Location, project: &Path) -> Inventory {
    let mut reader = Reader {
        provider: location.provider(),
        account: account.clone(),
        inventory: Inventory::default(),
        project: project.to_owned(),
    };
    match location {
        Location::Claude {
            config_dir,
            global_file,
        } => reader.claude(config_dir, global_file, project),
        Location::Codex { home, user_home } => reader.codex(home, user_home.as_deref(), project),
    }
    let mut inventory = reader.inventory;
    inventory.items.sort_by(|a, b| {
        (a.kind(), &a.name, a.scope, &a.source).cmp(&(b.kind(), &b.name, b.scope, &b.source))
    });
    inventory
}

/// `url` reduced to its scheme, host and port, with `/…` when it goes on: credentials,
/// any path segment, the query and the fragment can all hold a token. A URL that does
/// not parse is hidden whole.
pub fn redact_url(url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url) else {
        return "\u{2022}\u{2022}\u{2022}".into();
    };
    let origin = parsed.origin().ascii_serialization();
    if origin == "null" {
        return format!("{}:\u{2026}", parsed.scheme());
    }
    let more = parsed.path() != "/" || parsed.query().is_some() || parsed.fragment().is_some();
    if more {
        format!("{origin}/\u{2026}")
    } else {
        origin
    }
}

fn folder_hash(root: &Path, at: &Path, hasher: &mut Sha256) -> io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(root.join(at))?.collect::<Result<_, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let name = at.join(entry.file_name());
        hasher.update(name.to_string_lossy().as_bytes());
        hasher.update([0]);
        let kind = entry.file_type()?;
        if kind.is_dir() {
            folder_hash(root, &name, hasher)?;
        } else if kind.is_symlink() {
            hasher.update(fs::read_link(entry.path())?.to_string_lossy().as_bytes());
        } else {
            let bytes = fs::read(entry.path())?;
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        }
    }
    Ok(())
}

struct Reader {
    provider: Provider,
    account: AccountId,
    inventory: Inventory,
    project: PathBuf,
}

impl Reader {
    fn problem(&mut self, path: &Path, message: impl Into<String>) {
        self.inventory.problems.push(Problem {
            path: path.to_owned(),
            message: message.into(),
        });
    }

    fn push(&mut self, name: &str, scope: Scope, source: &Path, state: State, detail: Detail) {
        self.inventory.items.push(Item {
            provider: self.provider,
            account: self.account.clone(),
            name: name.to_owned(),
            scope,
            source: source.to_owned(),
            state,
            overridden_by: None,
            detail,
        });
    }

    /// The file's text, or `None` when it is missing or unreadable.
    fn text(&mut self, path: &Path) -> Option<String> {
        match fs::read_to_string(path) {
            Ok(text) => {
                let print = fingerprint(path, Some(&text), &self.project);
                self.inventory.files.insert(path.to_owned(), print);
                Some(text)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.inventory.files.insert(path.to_owned(), None);
                None
            }
            Err(error) => {
                self.problem(path, error.to_string());
                None
            }
        }
    }

    fn json(&mut self, path: &Path) -> Option<Value> {
        let text = self.text(path)?;
        match serde_json::from_str::<Value>(&text) {
            Ok(value) if value.is_object() => Some(value),
            Ok(_) => {
                self.problem(path, "is not a JSON object");
                None
            }
            Err(error) => {
                self.problem(path, error.to_string());
                None
            }
        }
    }

    fn toml(&mut self, path: &Path) -> Option<toml::Table> {
        let text = self.text(path)?;
        match toml::from_str::<toml::Table>(&text) {
            Ok(table) => Some(table),
            Err(error) => {
                self.problem(path, error.message().to_owned());
                None
            }
        }
    }

    fn claude(&mut self, config_dir: &Path, global_file: &Path, project: &Path) {
        let roots = project_roots(project);
        let global = self.json(global_file).unwrap_or_default();
        let entry = roots
            .iter()
            .rev()
            .map(|root| project_entry(&global["projects"], root))
            .find(|entry| !entry.is_null())
            .unwrap_or(&Value::Null);
        let disabled = strings(&entry["disabledMcpServers"]);
        let mut disabled_json = strings(&entry["disabledMcpjsonServers"]);
        let local_settings = project.join(".claude/settings.local.json");
        if let Some(settings) = self.json(&local_settings) {
            disabled_json.extend(strings(&settings["disabledMcpjsonServers"]));
        }
        let state = |name: &str, extra: &[String]| {
            if disabled.iter().chain(extra).any(|n| n == name) {
                State::Off
            } else {
                State::On
            }
        };
        let mut servers = Vec::new();
        for (scope, file, value) in [
            (Scope::User, global_file.to_owned(), &global["mcpServers"]),
            (Scope::Local, global_file.to_owned(), &entry["mcpServers"]),
        ] {
            servers.extend(
                value
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(name, server)| {
                        let state = state(name, &[]);
                        (scope, file.clone(), name.clone(), server.clone(), state)
                    }),
            );
        }
        for root in &roots {
            let mcp_json = root.join(".mcp.json");
            let Some(value) = self.json(&mcp_json) else {
                continue;
            };
            for (name, server) in value["mcpServers"].as_object().into_iter().flatten() {
                let state = state(name, &disabled_json);
                servers.push((
                    Scope::Project,
                    mcp_json.clone(),
                    name.clone(),
                    server.clone(),
                    state,
                ));
            }
        }
        for (scope, file, name, server, state) in servers {
            match claude_server(&server) {
                Some(server) => self.push(&name, scope, &file, state, Detail::McpServer(server)),
                None => self.problem(
                    &file,
                    format!("MCP server \u{201c}{name}\u{201d} has neither a command nor a URL"),
                ),
            }
        }

        let installed_file = config_dir.join("plugins/installed_plugins.json");
        let installed = self.json(&installed_file).unwrap_or_default();
        let installs: Vec<(String, Scope, Option<String>, Option<PathBuf>)> = installed["plugins"]
            .as_object()
            .into_iter()
            .flatten()
            .flat_map(|(id, installs)| {
                let roots = &roots;
                installs
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(move |install| {
                        let scope = match install["scope"].as_str()? {
                            "user" => Scope::User,
                            "project" => Scope::Project,
                            "local" => Scope::Local,
                            _ => return None,
                        };
                        let here = match install["projectPath"].as_str() {
                            Some(path) => roots.iter().any(|root| same_dir(Path::new(path), root)),
                            None => scope == Scope::User,
                        };
                        here.then(|| {
                            let version = install["version"].as_str().map(str::to_owned);
                            let path = install["installPath"].as_str().map(PathBuf::from);
                            (id.clone(), scope, version, path)
                        })
                    })
            })
            .collect();
        let install = |id: &str, scope: Scope| {
            installs
                .iter()
                .find(|(i, s, ..)| i == id && *s == scope)
                .or_else(|| installs.iter().find(|(i, ..)| i == id))
        };
        let mut declared = HashSet::new();
        for (scope, file) in [
            (Scope::User, config_dir.join("settings.json")),
            (Scope::Project, project.join(".claude/settings.json")),
            (Scope::Local, project.join(".claude/settings.local.json")),
        ] {
            let Some(settings) = self.json(&file) else {
                continue;
            };
            for (id, enabled) in settings["enabledPlugins"].as_object().into_iter().flatten() {
                declared.insert((id.clone(), scope));
                let state = if enabled.as_bool() == Some(false) {
                    State::Off
                } else {
                    State::On
                };
                let version = install(id, scope).and_then(|(_, _, version, _)| version.clone());
                self.push(id, scope, &file, state, Detail::Plugin { version });
            }
        }
        for (id, scope, version, _) in &installs {
            if declared.insert((id.clone(), *scope)) {
                let version = version.clone();
                self.push(
                    id,
                    *scope,
                    &installed_file,
                    State::Off,
                    Detail::Plugin { version },
                );
            }
        }

        self.mark_overrides();
        let off = HashSet::new();
        for (id, scope) in self.loaded_plugins() {
            if let Some((.., Some(folder))) = install(&id, scope) {
                self.skills(&folder.join("skills"), scope, &off, Some(&id));
            }
        }
        self.skills(&config_dir.join("skills"), Scope::User, &off, None);
        self.skills(&project.join(".claude/skills"), Scope::Project, &off, None);
    }

    fn codex(&mut self, home: &Path, user_home: Option<&Path>, project: &Path) {
        let roots = project_roots(project);
        let config_file = home.join("config.toml");
        let config = self.toml(&config_file).unwrap_or_default();
        let projects = config.get("projects").and_then(toml::Value::as_table);
        let trusted = roots
            .iter()
            .find_map(|root| {
                projects
                    .into_iter()
                    .flatten()
                    .find(|(path, _)| same_dir(Path::new(path), root))
            })
            .is_some_and(|(_, settings)| {
                settings.get("trust_level").and_then(toml::Value::as_str) == Some("trusted")
            });
        self.codex_servers(&config, Scope::User, &config_file, true);
        let user_skills = user_home.map(|home| home.join(".agents/skills"));
        for root in &roots {
            let project_file = root.join(".codex/config.toml");
            if same_dir(&project_file, &config_file) {
                continue;
            }
            if let Some(project_config) = self.toml(&project_file) {
                self.codex_servers(&project_config, Scope::Project, &project_file, trusted);
            }
        }

        for (id, plugin) in config
            .get("plugins")
            .and_then(toml::Value::as_table)
            .into_iter()
            .flatten()
        {
            let state = match plugin.get("enabled").and_then(toml::Value::as_bool) {
                Some(false) => State::Off,
                _ => State::On,
            };
            self.push(
                id,
                Scope::User,
                &config_file,
                state,
                Detail::Plugin { version: None },
            );
        }

        self.mark_overrides();
        let off: HashSet<PathBuf> = config
            .get("skills")
            .and_then(|skills| skills.get("config"))
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter(|entry| entry.get("enabled").and_then(toml::Value::as_bool) == Some(false))
            .filter_map(|entry| entry.get("path")?.as_str().map(PathBuf::from))
            .collect();
        for (id, scope) in self.loaded_plugins() {
            let Some((name, marketplace)) = id.split_once('@') else {
                continue;
            };
            let versions = home.join("plugins/cache").join(marketplace).join(name);
            let newest = fs::read_dir(&versions)
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .max_by_key(|path| version_key(path));
            if let Some(newest) = newest {
                self.skills(&newest.join("skills"), scope, &off, Some(&id));
            }
        }
        self.skills(&home.join("skills"), Scope::User, &off, None);
        if let Some(user_skills) = &user_skills {
            self.skills(user_skills, Scope::User, &off, None);
        }
        for root in &roots {
            let repo_skills = root.join(".agents/skills");
            if user_skills
                .as_deref()
                .is_some_and(|user| same_dir(user, &repo_skills))
            {
                continue;
            }
            self.skills(&repo_skills, Scope::Project, &off, None);
            let codex_skills = root.join(".codex/skills");
            if !same_dir(&codex_skills, &home.join("skills")) {
                self.skills(&codex_skills, Scope::Project, &off, None);
            }
        }
    }

    fn codex_servers(&mut self, config: &toml::Table, scope: Scope, file: &Path, trusted: bool) {
        let servers = config.get("mcp_servers").and_then(toml::Value::as_table);
        for (name, server) in servers.into_iter().flatten() {
            let Some(server) = server.as_table() else {
                self.problem(
                    file,
                    format!("MCP server \u{201c}{name}\u{201d} is not a table"),
                );
                continue;
            };
            let Some(detail) = codex_server(server) else {
                self.problem(
                    file,
                    format!("MCP server \u{201c}{name}\u{201d} has neither a command nor a URL"),
                );
                continue;
            };
            let state = if !trusted {
                State::Untrusted
            } else if server.get("enabled").and_then(toml::Value::as_bool) == Some(false) {
                State::Off
            } else {
                State::On
            };
            self.push(name, scope, file, state, Detail::McpServer(detail));
        }
    }

    /// Marks each MCP server or plugin that a declaration of the same name at a
    /// stronger scope replaces: local over project over user, and at one scope the
    /// file nearest the project. Skills are separate folders and replace nothing. A
    /// project file Codex does not trust, and a Claude plugin installed with no
    /// `enabledPlugins` entry, are not settings and replace nothing either.
    fn mark_overrides(&mut self) {
        let rank = |scope: Scope| match scope {
            Scope::User => 0,
            Scope::Project => 1,
            Scope::Local => 2,
        };
        let counts = |item: &Item| {
            item.kind() != Kind::Skill
                && item.state != State::Untrusted
                && !item.source.ends_with("plugins/installed_plugins.json")
        };
        let items = &self.inventory.items;
        let overridden: Vec<Option<Scope>> = items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                if !counts(item) {
                    return None;
                }
                items
                    .iter()
                    .enumerate()
                    .filter(|&(j, other)| {
                        counts(other)
                            && other.kind() == item.kind()
                            && other.name == item.name
                            && (rank(other.scope), std::cmp::Reverse(j))
                                > (rank(item.scope), std::cmp::Reverse(i))
                    })
                    .map(|(_, other)| other.scope)
                    .max_by_key(|&scope| rank(scope))
            })
            .collect();
        for (item, by) in self.inventory.items.iter_mut().zip(overridden) {
            item.overridden_by = by;
        }
    }

    /// Each plugin the provider loads, with the scope of the declaration that turns it
    /// on.
    fn loaded_plugins(&self) -> Vec<(String, Scope)> {
        self.inventory
            .items
            .iter()
            .filter(|item| {
                item.kind() == Kind::Plugin
                    && item.state == State::On
                    && item.overridden_by.is_none()
            })
            .map(|item| (item.name.clone(), item.scope))
            .collect()
    }

    /// Each `<folder>/<name>/SKILL.md` in `root`, skipping hidden folders such as
    /// Codex's bundled `.system`. A skill whose `SKILL.md` is in `off` is off. A
    /// plugin's skills are named `plugin:skill`, as both providers list them.
    fn skills(&mut self, root: &Path, scope: Scope, off: &HashSet<PathBuf>, plugin: Option<&str>) {
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) => return self.problem(root, error.to_string()),
        };
        let mut folders: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
            .map(|entry| entry.path())
            .filter(|path| path.join("SKILL.md").is_file())
            .collect();
        folders.sort();
        for folder in folders {
            let file = folder.join("SKILL.md");
            let Some(text) = self.text(&file) else {
                continue;
            };
            if plugin.is_none()
                && let Ok(print) = fingerprint_now(&folder, &self.project)
            {
                self.inventory.files.insert(folder.clone(), print);
            }
            let folder_name = folder
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mut name = front_matter(&text, "name").unwrap_or(folder_name);
            if let Some(plugin) = plugin {
                name = format!("{}:{name}", plugin.split('@').next().unwrap_or(plugin));
            }
            let description = front_matter(&text, "description").unwrap_or_default();
            let state = if off.contains(&file) {
                State::Off
            } else {
                State::On
            };
            let detail = Detail::Skill {
                description,
                plugin: plugin.map(str::to_owned),
            };
            self.push(&name, scope, &folder, state, detail);
        }
    }
}

/// The project folder, then each folder above it up to its git root, which is where
/// Claude keys a project and the furthest it looks for `.mcp.json`. A folder outside
/// git is alone. When the git root is the user's home, the account's own files there
/// are still read once, at user scope.
fn project_roots(project: &Path) -> Vec<PathBuf> {
    let ancestors: Vec<PathBuf> = project.ancestors().map(Path::to_owned).collect();
    match ancestors
        .iter()
        .position(|folder| folder.join(".git").exists())
    {
        Some(git_root) => ancestors[..=git_root].to_vec(),
        None => vec![project.to_owned()],
    }
}

/// Claude's entry for `project` in `.claude.json`'s `projects`, keyed by the folder's
/// absolute path.
fn project_entry<'a>(projects: &'a Value, project: &Path) -> &'a Value {
    projects
        .as_object()
        .and_then(|projects| {
            projects
                .iter()
                .find(|(path, _)| same_dir(Path::new(path), project))
                .map(|(_, entry)| entry)
        })
        .unwrap_or(&Value::Null)
}

/// A cached plugin folder's name as a version: its dot-separated numbers compare as
/// numbers, so `1.10.0` sorts after `1.9.0`.
fn version_key(path: &Path) -> Vec<(u64, String)> {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
        .split(['.', '-', '+'])
        .map(|part| (part.parse().unwrap_or(0), part.to_owned()))
        .collect()
}

/// Whether two paths name the same folder, by text or after resolving symlinks.
fn same_dir(a: &Path, b: &Path) -> bool {
    let trim = |p: &Path| p.components().collect::<PathBuf>();
    trim(a) == trim(b)
        || matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(a), Ok(b)) if a == b)
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect()
}

fn keys(value: &Value) -> Vec<String> {
    value
        .as_object()
        .into_iter()
        .flatten()
        .map(|(key, _)| key.clone())
        .collect()
}

/// A stdio transport that shows only `command`'s first word: a whole command line such
/// as `env TOKEN=… node` can be pasted there.
fn stdio(command: &str, args: usize) -> Transport {
    let mut words = command.split_whitespace();
    Transport::Stdio {
        command: words.next().unwrap_or_default().to_owned(),
        args: args + words.count(),
    }
}

/// A server from `.claude.json` or `.mcp.json`. A `command` without a `type` is stdio,
/// and a `url` without one is HTTP.
fn claude_server(server: &Value) -> Option<McpServer> {
    let kind = server["type"].as_str();
    let transport = match (kind, server["command"].as_str(), server["url"].as_str()) {
        (None | Some("stdio"), Some(command), _) => {
            stdio(command, server["args"].as_array().map_or(0, Vec::len))
        }
        (kind, _, Some(url)) if kind != Some("stdio") => Transport::Remote {
            kind: kind.unwrap_or("http").to_owned(),
            url: redact_url(url),
        },
        _ => return None,
    };
    Some(McpServer {
        transport,
        env: keys(&server["env"]),
        headers: keys(&server["headers"]),
    })
}

/// A server from Codex's `[mcp_servers.<name>]`. `env` and `env_vars` both give it
/// variables; `http_headers`, `env_http_headers` and `bearer_token_env_var` headers.
fn codex_server(server: &toml::Table) -> Option<McpServer> {
    let text = |key: &str| server.get(key).and_then(toml::Value::as_str);
    let table_keys = |key: &str| -> Vec<String> {
        server
            .get(key)
            .and_then(toml::Value::as_table)
            .into_iter()
            .flat_map(|table| table.keys().cloned())
            .collect()
    };
    let list = |key: &str| -> Vec<String> {
        server
            .get(key)
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()
    };
    let transport = match (text("command"), text("url")) {
        (Some(command), _) => stdio(
            command,
            server
                .get("args")
                .and_then(toml::Value::as_array)
                .map_or(0, Vec::len),
        ),
        (None, Some(url)) => Transport::Remote {
            kind: "http".into(),
            url: redact_url(url),
        },
        (None, None) => return None,
    };
    let mut env = table_keys("env");
    env.extend(list("env_vars"));
    let mut headers = table_keys("http_headers");
    headers.extend(table_keys("env_http_headers"));
    if text("bearer_token_env_var").is_some() {
        headers.push("Authorization".into());
    }
    Some(McpServer {
        transport,
        env,
        headers,
    })
}

/// A `key: value` line in the YAML front matter that opens `text`, unquoted. A folded
/// or literal value (`>` or `|`) is its indented lines joined by spaces.
fn front_matter(text: &str, key: &str) -> Option<String> {
    let mut lines = text.lines();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    let mut lines = lines.take_while(|line| line.trim_end() != "---").peekable();
    while let Some(line) = lines.next() {
        let Some(value) = line
            .strip_prefix(key)
            .and_then(|rest| rest.trim_start().strip_prefix(':'))
        else {
            continue;
        };
        let value = value.trim();
        let value = if value.is_empty() || value.starts_with(['>', '|']) {
            let mut folded = Vec::new();
            while let Some(next) = lines.next_if(|l| l.starts_with([' ', '\t']) || l.is_empty()) {
                folded.push(next.trim());
            }
            folded.join(" ").trim().to_owned()
        } else {
            value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(value)
                .to_owned()
        };
        return (!value.is_empty()).then_some(value);
    }
    None
}

#[cfg(test)]
#[path = "integrations_tests.rs"]
mod tests;
