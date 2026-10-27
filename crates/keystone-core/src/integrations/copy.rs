//! One-shot copies of MCP servers and skill folders from one account to another, of
//! the same provider or the other one. A copy adds the server or folder through
//! [`edit`] and leaves a [`Pair`], which is only compared later and never kept in sync.
//!
//! Between providers a server keeps what both can express: a stdio command, its
//! arguments and environment, or an HTTP URL and its headers. Codex's variables passed
//! by name (`env_vars`, `env_http_headers`, `bearer_token_env_var`) become Claude's
//! `${VAR}` references and back. Anything else only one provider reads stays behind.
//!
//! A value that looks like a secret is masked in the preview and copied only when the
//! user includes it. One left out is listed on the pair, so the copy can say it needs
//! it.

use super::edit::{self, Change};
use super::{Detail, Item, Kind, Location, Scope, fingerprint_now, same_dir};
use crate::{AccountId, Provider};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// What the preview shows in place of a secret.
pub const MASK: &str = "\u{2022}\u{2022}\u{2022}\u{2022}";

/// One end of a copy: a server's declaration or a skill's folder.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct End {
    pub provider: Provider,
    pub account: AccountId,
    pub kind: Kind,
    pub scope: Scope,
    pub name: String,
    /// The file that declares the server, or the skill's folder.
    pub source: PathBuf,
}

impl End {
    pub fn of(item: &Item) -> Self {
        Self {
            provider: item.provider,
            account: item.account.clone(),
            kind: item.kind(),
            scope: item.scope,
            name: item.name.clone(),
            source: item.source.clone(),
        }
    }

    pub fn is(&self, item: &Item) -> bool {
        (
            self.provider,
            &self.account,
            self.kind,
            self.scope,
            &self.name,
        ) == (
            item.provider,
            &item.account,
            item.kind(),
            item.scope,
            &item.name,
        ) && same_dir(&self.source, &item.source)
    }
}

/// A copy Keystone made.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Pair {
    pub from: End,
    pub to: End,
    /// The secrets the user left out, by [`Preview::secrets`] label.
    pub left_out: Vec<String>,
    /// The project it was made in, which a `Local` end belongs to.
    pub project: PathBuf,
}

/// What a copy will write, before it is made. Two previews of one copy are equal while
/// its source still gives what the first showed, a skill folder's content included; an
/// edit to the rest of the source's file is not a change.
#[derive(Clone, Debug, PartialEq)]
pub struct Preview {
    /// The file or folder it writes.
    pub to: PathBuf,
    /// What it writes there, each included secret masked: the server's definition, or
    /// the skill's files one per line.
    pub text: String,
    /// Each value that looks like a secret, by the name of its variable, header or
    /// flag, or by where it is: `Argument 2`, `URL` or `Command`.
    pub secrets: Vec<String>,
    pub change: Change,
    pub pair: Pair,
}

/// How a pair's two ends compare now.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Drift {
    Same,
    /// Either end changed since the copy.
    Differs,
    /// The same, but the copy still lacks these secrets the user left out.
    Needs(Vec<String>),
    /// Either end is no longer there.
    Gone,
}

/// What a provider's own items that cannot be copied are labelled.
pub fn only(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "Claude only",
        Provider::Codex => "Codex only",
    }
}

/// The copy of `item`, which `from` lists, to the account `to` names at `scope`, for
/// `project`, with the secrets `include` names. An `Err` is a copy that cannot be made.
pub fn preview(
    item: &Item,
    from: &Location,
    (to, account, scope): (&Location, &AccountId, Scope),
    project: &Path,
    include: &[String],
) -> Result<Preview, String> {
    let end = |source: PathBuf| End {
        provider: to.provider(),
        account: account.clone(),
        kind: item.kind(),
        scope,
        name: item.name.clone(),
        source,
    };
    match &item.detail {
        Detail::Plugin { .. }
        | Detail::Skill {
            plugin: Some(_), ..
        } => Err(only(item.provider).into()),
        Detail::Skill { .. } => {
            let name = item.source.file_name().ok_or("Choose a skill folder.")?;
            let folder = edit::skills_root(to, project, scope)?.join(name);
            if folder.exists() {
                return Err(format!(
                    "A skill named {} is already there.",
                    name.to_string_lossy()
                ));
            }
            let print =
                fingerprint_now(&item.source, project).map_err(|error| error.to_string())?;
            if print.is_none() {
                return Err(format!("{} is no longer there.", item.name));
            }
            let mut files = Vec::new();
            if list_files(&item.source, Path::new(""), &mut files) {
                return Err(format!("{} links outside its folder.", item.name));
            }
            Ok(Preview {
                to: folder.clone(),
                text: files.join("\n"),
                secrets: Vec::new(),
                change: Change::AddSkill {
                    scope,
                    folder: item.source.clone(),
                    content: print,
                },
                pair: Pair {
                    from: End::of(item),
                    to: end(folder),
                    left_out: Vec::new(),
                    project: project.to_owned(),
                },
            })
        }
        Detail::McpServer(_) => {
            let source = definition(from, project, &End::of(item))
                .ok_or_else(|| format!("{} is no longer there.", item.name))?;
            let file = server_file(to, project, scope)?;
            let target = end(file.clone());
            if definition(to, project, &target).is_some() {
                return Err(format!("A server named {} is already there.", item.name));
            }
            let copied = translate(&source, item.provider, to.provider())?;
            let secrets = secrets(&copied, to.provider());
            let left_out = |secret: &Secret| !include.contains(&secret.label);
            let written = leave_out(&copied, &secrets, left_out);
            let shown = mask(&written, &self::secrets(&written, to.provider()));
            let text = match to {
                Location::Claude { .. } => {
                    let named = Value::Object(Map::from_iter([(item.name.clone(), shown)]));
                    serde_json::to_string_pretty(&named).map_err(|error| error.to_string())?
                }
                Location::Codex { .. } => edit::codex_text(&item.name, &shown)?,
            };
            let mut labels: Vec<String> = Vec::new();
            for secret in &secrets {
                if !labels.contains(&secret.label) {
                    labels.push(secret.label.clone());
                }
            }
            Ok(Preview {
                to: file,
                text,
                change: Change::AddDefinition {
                    scope,
                    name: item.name.clone(),
                    definition: written,
                },
                pair: Pair {
                    from: End::of(item),
                    to: target,
                    left_out: labels
                        .iter()
                        .filter(|label| !include.contains(label))
                        .cloned()
                        .collect(),
                    project: project.to_owned(),
                },
                secrets: labels,
            })
        }
    }
}

/// How `pair`'s ends compare now in `project`, `from` and `to` naming their accounts'
/// files, or `None` when the pair is another project's `Local` server.
pub fn drift(pair: &Pair, from: &Location, to: &Location, project: &Path) -> Option<Drift> {
    let local = [&pair.from, &pair.to]
        .iter()
        .any(|end| end.scope == Scope::Local);
    if local && !same_dir(&pair.project, project) {
        return None;
    }
    if pair.to.kind == Kind::Skill {
        let print = |end: &End| fingerprint_now(&end.source, project).ok();
        return Some(match (print(&pair.from)?, print(&pair.to)?) {
            (Some(source), Some(copy)) if source == copy => Drift::Same,
            (Some(_), Some(_)) => Drift::Differs,
            _ => Drift::Gone,
        });
    }
    let (Some(source), Some(copy)) = (
        read_end(from, project, &pair.from)?,
        read_end(to, project, &pair.to)?,
    ) else {
        return Some(Drift::Gone);
    };
    let Ok(expected) = translate(&source, from.provider(), to.provider()) else {
        return Some(Drift::Differs);
    };
    // An argument is named by its place, which leaving one out moves, so when any was
    // left out every one is compared by count rather than by place.
    let positional = |secret: &Secret| matches!(secret.place, Place::Arg(Word::Whole(_)));
    let wanted = secrets(&expected, to.provider());
    let present = secrets(&copy, to.provider());
    let left_out = |secret: &Secret| pair.left_out.contains(&secret.label);
    let by_count = wanted
        .iter()
        .any(|secret| positional(secret) && left_out(secret));
    let skip = |secret: &Secret| {
        if positional(secret) {
            by_count
        } else {
            left_out(secret)
        }
    };
    // A URL's or the command's secrets share one label, so it is filled only once every
    // one of them is back.
    let (source_masked, copy_masked) = (mask(&expected, &wanted), mask(&copy, &present));
    let expected = leave_out(&expected, &wanted, skip);
    let args_filled = present.iter().filter(|s| positional(s)).count()
        >= wanted.iter().filter(|s| positional(s)).count();
    let mut actual = copy.clone();
    let mut needs = Vec::new();
    for label in &pair.left_out {
        let filled = match wanted.iter().find(|secret| &secret.label == label) {
            Some(secret) if positional(secret) => args_filled,
            Some(Secret {
                place: Place::Url, ..
            }) => source_masked["url"] == copy_masked["url"],
            Some(Secret {
                place: Place::Command(_),
                ..
            }) => source_masked["command"] == copy_masked["command"],
            _ => present
                .iter()
                .any(|secret| !positional(secret) && &secret.label == label),
        };
        if !filled && !forget(&mut actual, label) {
            needs.push(label.clone());
        }
    }
    let actual = leave_out(&actual, &present, skip);
    Some(if normal(expected) != normal(actual) {
        Drift::Differs
    } else if needs.is_empty() {
        Drift::Same
    } else {
        Drift::Needs(needs)
    })
}

/// Takes out of `definition` what gives the variable or header `label` a value by
/// reference, as a user fills in a secret a copy left out: `${VAR}`, or Codex's
/// `env_vars`, `env_http_headers` and `bearer_token_env_var`. Whether there was any.
fn forget(definition: &mut Value, label: &str) -> bool {
    let Some(object) = definition.as_object_mut() else {
        return false;
    };
    let mut found = false;
    for table in ["env", "headers", "http_headers", "env_http_headers"] {
        if let Some(entries) = object.get_mut(table).and_then(Value::as_object_mut) {
            found |= entries.remove(label).is_some();
        }
    }
    if let Some(names) = object.get_mut("env_vars").and_then(Value::as_array_mut) {
        let before = names.len();
        names.retain(|name| name.as_str() != Some(label));
        found |= names.len() != before;
    }
    if label.eq_ignore_ascii_case("authorization") {
        found |= object.remove("bearer_token_env_var").is_some();
    }
    prune(definition);
    found
}

/// The file a server added at `scope` is declared in.
fn server_file(location: &Location, project: &Path, scope: Scope) -> Result<PathBuf, String> {
    Ok(match (location, scope) {
        (Location::Claude { .. }, Scope::Project) => project.join(".mcp.json"),
        (Location::Claude { global_file, .. }, _) => global_file.clone(),
        (Location::Codex { home, .. }, Scope::User) => home.join("config.toml"),
        (Location::Codex { .. }, Scope::Project) => project.join(".codex/config.toml"),
        (Location::Codex { .. }, Scope::Local) => return Err(edit::NO_LOCAL.into()),
    })
}

/// The server `end` names as its file has it now: `Some(None)` when the file is
/// gone or no longer declares it, `None` when the file cannot be read or parsed.
fn read_end(location: &Location, project: &Path, end: &End) -> Option<Option<Value>> {
    if let Some(server) = definition(location, project, end) {
        return Some(Some(server));
    }
    match fs::read_to_string(&end.source) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(None),
        Err(_) => None,
        Ok(text) => {
            let parses = match location {
                Location::Claude { .. } => serde_json::from_str::<Value>(&text).is_ok(),
                Location::Codex { .. } => toml::from_str::<toml::Table>(&text).is_ok(),
            };
            parses.then_some(None)
        }
    }
}

/// The server `end` names as its file has it now, secrets included.
fn definition(location: &Location, project: &Path, end: &End) -> Option<Value> {
    match location {
        Location::Claude { global_file, .. } => {
            edit::claude_server(global_file, project, &end.source, end.scope, &end.name)
        }
        Location::Codex { .. } => edit::codex_server(&end.source, &end.name),
    }
}

/// `${NAME}`'s variable name.
fn reference(value: &str) -> Option<&str> {
    let name = value.strip_prefix("${")?.strip_suffix('}')?;
    let valid = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    valid.then_some(name)
}

/// The variable a `Bearer ${NAME}` value names.
fn bearer_reference(value: &str) -> Option<&str> {
    let bearer = value.get(..7)?.eq_ignore_ascii_case("bearer ");
    bearer.then(|| reference(&value[7..])).flatten()
}

/// Whether `text` names a variable, as `${NAME}` or `$NAME`, rather than holding a value.
fn variable(text: &str) -> bool {
    reference(text).is_some() || text.strip_prefix('$').is_some_and(env_name)
}

fn object(value: &Value) -> impl Iterator<Item = (&String, &Value)> {
    value.as_object().into_iter().flatten()
}

/// The first field of `object`, or entry of one of its tables, whose text holds
/// `${`.
fn referenced(object: &Map<String, Value>) -> Option<String> {
    let has = |value: &Value| strings(value).iter().any(|text| text.contains("${"));
    object.iter().find_map(|(key, value)| match value {
        Value::Object(entries) => entries
            .iter()
            .find(|(_, value)| has(value))
            .map(|(name, _)| name.clone()),
        value => has(value).then(|| key.clone()),
    })
}

/// Every string in `value`.
fn strings(value: &Value) -> Vec<&str> {
    match value {
        Value::String(text) => vec![text],
        Value::Array(list) => list.iter().flat_map(strings).collect(),
        Value::Object(object) => object.values().flat_map(strings).collect(),
        _ => Vec::new(),
    }
}

/// `definition`, which `from` keeps, as `to` keeps a server. Codex's own switch stays
/// behind, as Claude's does: a copy arrives on.
fn translate(definition: &Value, from: Provider, to: Provider) -> Result<Value, String> {
    if from == to {
        let mut copy = definition.clone();
        if let Some(object) = copy.as_object_mut() {
            object.remove("enabled");
        }
        return Ok(copy);
    }
    let mut out = Map::new();
    let args = definition["args"]
        .as_array()
        .filter(|args| !args.is_empty())
        .cloned();
    let table = |entries: Map<String, Value>| (!entries.is_empty()).then(|| Value::from(entries));
    if from == Provider::Claude {
        let kind = definition["type"].as_str();
        if kind == Some("sse") {
            return Err("Codex can't use SSE servers.".into());
        }
        if let (None | Some("stdio"), Some(command)) = (kind, definition["command"].as_str()) {
            let (mut env, mut vars) = (Map::new(), Vec::new());
            for (name, value) in object(&definition["env"]) {
                match value.as_str().and_then(reference) {
                    Some(variable) if variable == name => vars.push(Value::from(name.clone())),
                    _ => {
                        env.insert(name.clone(), value.clone());
                    }
                }
            }
            out.insert("command".into(), command.into());
            out.extend(args.map(|args| ("args".to_owned(), args.into())));
            out.extend(table(env).map(|env| ("env".to_owned(), env)));
            if !vars.is_empty() {
                out.insert("env_vars".into(), vars.into());
            }
        } else if let Some(url) = definition["url"].as_str() {
            let (mut headers, mut from_env) = (Map::new(), Map::new());
            for (name, value) in object(&definition["headers"]) {
                let text = value.as_str().unwrap_or_default();
                let bearer =
                    bearer_reference(text).filter(|_| name.eq_ignore_ascii_case("authorization"));
                if let Some(variable) = bearer {
                    out.insert("bearer_token_env_var".into(), variable.into());
                } else if let Some(variable) = reference(text) {
                    from_env.insert(name.clone(), variable.into());
                } else {
                    headers.insert(name.clone(), value.clone());
                }
            }
            out.insert("url".into(), url.into());
            out.extend(table(headers).map(|headers| ("http_headers".to_owned(), headers)));
            out.extend(table(from_env).map(|from_env| ("env_http_headers".to_owned(), from_env)));
        } else {
            return Err("Codex needs a command or a URL.".into());
        }
        return match referenced(&out) {
            Some(field) => Err(format!("Codex can't expand the reference in {field}.")),
            None => Ok(out.into()),
        };
    }
    if let Some(field) = definition.as_object().and_then(referenced) {
        return Err(format!("Claude would expand the ${{…}} in {field}."));
    }
    if let Some(key) = ["enabled_tools", "disabled_tools", "cwd"]
        .into_iter()
        .find(|key| !definition[*key].is_null())
    {
        return Err(format!("Claude has no {key}."));
    }
    if let Some(command) = definition["command"].as_str() {
        let mut env: Map<String, Value> = object(&definition["env"])
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        for name in definition["env_vars"].as_array().into_iter().flatten() {
            if let Some(name) = name.as_str() {
                env.insert(name.into(), format!("${{{name}}}").into());
            }
        }
        out.insert("type".into(), "stdio".into());
        out.insert("command".into(), command.into());
        out.extend(args.map(|args| ("args".to_owned(), args.into())));
        out.extend(table(env).map(|env| ("env".to_owned(), env)));
    } else if let Some(url) = definition["url"].as_str() {
        let mut headers: Map<String, Value> = object(&definition["http_headers"])
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        for (name, variable) in object(&definition["env_http_headers"]) {
            let variable = variable.as_str().unwrap_or_default();
            headers.insert(name.clone(), format!("${{{variable}}}").into());
        }
        if let Some(variable) = definition["bearer_token_env_var"].as_str() {
            headers.insert(
                "Authorization".into(),
                format!("Bearer ${{{variable}}}").into(),
            );
        }
        out.insert("type".into(), "http".into());
        out.insert("url".into(), url.into());
        out.extend(table(headers).map(|headers| ("headers".to_owned(), headers)));
    } else {
        return Err("Claude needs a command or a URL.".into());
    }
    Ok(out.into())
}

/// A value in a server's definition that looks like a secret.
#[derive(Clone, Debug)]
struct Secret {
    label: String,
    place: Place,
}

#[derive(Clone, Debug)]
enum Place {
    /// The entry `name` of the table `table`: an environment variable or a header.
    Entry { table: &'static str, name: String },
    /// A secret among the arguments.
    Arg(Word),
    /// Every secret among the command's words.
    Command(Vec<Word>),
    /// A password, a path segment or a query value in the URL.
    Url,
}

/// Where a secret is in a list of words: the arguments, or the command's words.
#[derive(Clone, Debug)]
enum Word {
    /// The word at this index, as `--flag=value` or `NAME=value`.
    Joined { at: usize, name: String },
    /// The flag at this index and the value after it.
    Flag(usize),
    /// The word at this index, on its own.
    Whole(usize),
}

impl Word {
    /// The index of the word that shows [`MASK`] for this secret, and what it shows.
    fn masked(&self) -> (usize, String) {
        match self {
            Word::Joined { at, name } => (*at, format!("{name}={MASK}")),
            Word::Flag(at) => (at + 1, MASK.into()),
            Word::Whole(at) => (*at, MASK.into()),
        }
    }

    /// The indexes of the words it takes up.
    fn span(&self) -> std::ops::Range<usize> {
        match *self {
            Word::Joined { at, .. } | Word::Whole(at) => at..at + 1,
            Word::Flag(at) => at..at + 2,
        }
    }
}

/// The words that, at the end of a part of a name, say it holds a secret.
const SECRET_WORDS: [&str; 10] = [
    "TOKEN",
    "KEY",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "PASS",
    "AUTH",
    "AUTHORIZATION",
    "CREDENTIALS",
    "COOKIE",
];

/// Whether a variable, header, flag or query parameter of this name holds a secret:
/// one of its parts ends in one of [`SECRET_WORDS`], as `API_KEY`, `--apikey` and
/// `OAUTH` do and `AUTHOR` and `--keymap` do not.
fn secret_name(name: &str) -> bool {
    name.to_ascii_uppercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|part| {
            SECRET_WORDS
                .iter()
                .any(|word| part == *word || (*word != "PASS" && part.ends_with(word)))
        })
}

/// Whether `text` holds a secret whatever it is named: a bearer or basic credential (one
/// word of eight characters or more after the scheme, at the start or after a colon and
/// any whitespace), a URL with a [`secret_segment`] as
/// its user name or in its path or with a password, or a word that starts as well-known
/// tokens do. A credential after its scheme that names a [`variable`] is not one.
fn secret_value(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let credential = ["bearer ", "basic "].iter().any(|scheme| {
        let start = lower.strip_prefix(scheme).map(|_| 0).or_else(|| {
            lower.match_indices(':').find_map(|(at, _)| {
                let after = lower[at + 1..].trim_start();
                after.starts_with(scheme).then(|| lower.len() - after.len())
            })
        });
        start.is_some_and(|at| {
            let rest = text[at + scheme.len()..].trim();
            rest.len() >= 8 && !rest.contains(char::is_whitespace) && !variable(rest)
        })
    });
    let password = text.find("://").is_some_and(|at| {
        let authority = text[at + 3..]
            .split(['/', '?', '#'])
            .next()
            .unwrap_or_default();
        authority.rsplit_once('@').is_some_and(|(user, _)| {
            let (name, password) = user.split_once(':').unwrap_or((user, ""));
            secret_segment(name) || (!password.is_empty() && reference(password).is_none())
        })
    });
    const PREFIXES: [&str; 9] = [
        "sk-",
        "sk_live_",
        "ghp_",
        "gho_",
        "github_pat_",
        "glpat-",
        "xoxb-",
        "xoxp-",
        "AKIA",
    ];
    let token = text
        .split(|c: char| c.is_whitespace() || c == '=' || c == ':' || c == '"')
        .any(|word| word.len() >= 16 && PREFIXES.iter().any(|prefix| word.starts_with(prefix)));
    let path = text.find("://").is_some_and(|at| {
        let rest = text[at + 3..].split(['?', '#']).next().unwrap_or_default();
        rest.split('/').skip(1).any(secret_segment)
    });
    credential || password || token || path
}

/// Whether `segment`, one part of a URL's path or its user name, is a secret once
/// percent-decoded and without the `=` that pads its end: a well-known token, a UUID,
/// a piece between `-`, `_`, `.`, `~` and `:` of 16 or more letters and digits that
/// holds both, or 20 or more letters, digits, `-`, `_`, `+` and `/` holding a capital, a
/// small letter and a digit, at least half of whose letters and digits are in pieces
/// between `-`, `_`, `+` and `/` that hold both a letter and a digit. A name, a model's
/// name, a version, a date or a number is not.
fn secret_segment(segment: &str) -> bool {
    let decoded = percent_encoding::percent_decode_str(segment).decode_utf8_lossy();
    let segment = decoded.trim_end_matches('=');
    let has = |text: &str, test: fn(&char) -> bool| text.chars().any(|c| test(&c));
    let uuid = segment.split('-').map(str::len).eq([8, 4, 4, 4, 12])
        && segment.chars().all(|c| c == '-' || c.is_ascii_hexdigit());
    let mixed = segment.split(['-', '_', '.', '~', ':']).any(|piece| {
        piece.len() >= 16
            && piece.chars().all(|c| c.is_ascii_alphanumeric())
            && has(piece, char::is_ascii_alphabetic)
            && has(piece, char::is_ascii_digit)
    });
    let separators = ['-', '_', '+', '/'];
    let in_mixed_pieces: usize = segment
        .split(separators)
        .filter(|&piece| has(piece, char::is_ascii_alphabetic) && has(piece, char::is_ascii_digit))
        .map(str::len)
        .sum();
    let long = segment.len() >= 20
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || separators.contains(&c))
        && has(segment, char::is_ascii_uppercase)
        && has(segment, char::is_ascii_lowercase)
        && has(segment, char::is_ascii_digit)
        && 2 * in_mixed_pieces >= segment.len() - segment.matches(separators).count();
    secret_value(segment) || uuid || mixed || long
}

/// Whether `name` reads as an environment variable's name, as in `env NAME=value`.
fn env_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `url` with its user and password when its user name is a [`secret_segment`], else its
/// password, each [`secret_segment`] of its path and each secret query value, and of a
/// fragment of the form `k=v&k=v` too, as `with`, or taken out (a user with its `@`)
/// when `with` is `None`.
fn url_secrets(url: &str, with: Option<&str>) -> String {
    let (head, fragment) = url
        .split_once('#')
        .map_or((url, None), |(h, f)| (h, Some(f)));
    let (head, query) = head
        .split_once('?')
        .map_or((head, None), |(h, q)| (h, Some(q)));
    let mut out = head.to_owned();
    if let Some(at) = head.find("://") {
        let rest = &head[at + 3..];
        let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        let mut kept = authority.to_owned();
        if let Some((user, host)) = authority.rsplit_once('@') {
            let (name, password) = user.split_once(':').unwrap_or((user, ""));
            if secret_segment(name) {
                kept = with.map_or(host.to_owned(), |with| format!("{with}@{host}"));
            } else if !password.is_empty() {
                kept = with.map_or(format!("{name}@{host}"), |with| {
                    format!("{name}:{with}@{host}")
                });
            }
        }
        let path: Vec<&str> = path
            .split('/')
            .filter_map(|segment| {
                if secret_segment(segment) {
                    with
                } else {
                    Some(segment)
                }
            })
            .collect();
        out = format!("{}{kept}{}", &head[..at + 3], path.join("/"));
    }
    let params = |part: &str| -> Vec<String> {
        part.split('&')
            .filter_map(|param| match param.split_once('=') {
                Some((key, value)) if secret_name(key) || secret_value(value) => {
                    with.map(|with| format!("{key}={with}"))
                }
                _ => Some(param.to_owned()),
            })
            .collect()
    };
    if let Some(query) = query {
        let params = params(query);
        if !params.is_empty() {
            out = format!("{out}?{}", params.join("&"));
        }
    }
    match fragment {
        Some(fragment) if fragment.split('&').all(|param| param.contains('=')) => {
            let params = params(fragment);
            if params.is_empty() {
                out
            } else {
                format!("{out}#{}", params.join("&"))
            }
        }
        Some(fragment) => format!("{out}#{fragment}"),
        None => out,
    }
}

/// Every value in `definition`, as `provider` keeps a server, that looks like a
/// secret, by its name or by itself. A `${VAR}` reference holds none.
fn secrets(definition: &Value, provider: Provider) -> Vec<Secret> {
    let tables: [&'static str; 2] = match provider {
        Provider::Claude => ["env", "headers"],
        Provider::Codex => ["env", "http_headers"],
    };
    let mut found = Vec::new();
    for table in tables {
        for (name, value) in object(&definition[table]) {
            let text = value.as_str().unwrap_or_default();
            let referenced = reference(text).or_else(|| bearer_reference(text)).is_some();
            if (secret_name(name) || secret_value(text)) && !referenced {
                found.push(Secret {
                    label: name.clone(),
                    place: Place::Entry {
                        table,
                        name: name.clone(),
                    },
                });
            }
        }
    }
    let command = shell_words(definition["command"].as_str().unwrap_or_default());
    let words: Vec<&str> = command.iter().map(|(word, _)| word.as_str()).collect();
    let hits = scan(&words);
    if !hits.is_empty() {
        found.push(Secret {
            label: "Command".into(),
            place: Place::Command(hits),
        });
    }
    let args: Vec<&str> = definition["args"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|arg| arg.as_str().unwrap_or_default())
        .collect();
    for word in scan(&args) {
        let label = match &word {
            Word::Joined { name, .. } => name.clone(),
            Word::Flag(at) => args[*at].to_owned(),
            Word::Whole(at) => format!("Argument {}", at + 1),
        };
        found.push(Secret {
            label,
            place: Place::Arg(word),
        });
    }
    if let Some(url) = definition["url"].as_str()
        && url_secrets(url, Some(MASK)) != url
    {
        found.push(Secret {
            label: "URL".into(),
            place: Place::Url,
        });
    }
    found
}

/// Each secret among `words`, a command's words or a server's arguments:
/// `NAME=value` or `--flag=value`, a flag and the value after it, the word of eight
/// characters or more after `Bearer` or a word ending in `:Bearer`, or after `Basic` when
/// the word before that ends in `Authorization:` or after a word ending in
/// `Authorization:Basic`, unless it names a [`variable`], a word that is one by itself, or
/// a JSON object holding a [`json_secret`].
fn scan(words: &[&str]) -> Vec<Word> {
    let mut found = Vec::new();
    let mut at = 0;
    while at < words.len() {
        let word = words[at];
        let flag = word.starts_with('-');
        let switch = word.starts_with("--no-");
        match word.split_once('=') {
            _ if at.checked_sub(1).is_some_and(|before| {
                let scheme = words[before].to_ascii_lowercase();
                scheme == "bearer"
                    || scheme.ends_with(":bearer")
                    || scheme.ends_with("authorization:basic")
                    || (scheme == "basic"
                        && before.checked_sub(1).is_some_and(|header| {
                            words[header]
                                .to_ascii_lowercase()
                                .ends_with("authorization:")
                        }))
            }) && word.len() >= 8
                && !word.starts_with('-')
                && !variable(word) =>
            {
                found.push(Word::Whole(at))
            }
            Some((name, value))
                if (flag || env_name(name))
                    && !value.is_empty()
                    && (secret_name(name) || secret_value(value)) =>
            {
                found.push(Word::Joined {
                    at,
                    name: name.to_owned(),
                })
            }
            None if flag
                && !switch
                && secret_name(word)
                && words.get(at + 1).is_some_and(|next| !next.starts_with('-')) =>
            {
                found.push(Word::Flag(at));
                at += 1;
            }
            _ if secret_value(word)
                || (word.contains("://") && url_secrets(word, Some(MASK)) != word)
                || serde_json::from_str::<Value>(word)
                    .is_ok_and(|value| value.is_object() && json_secret("", &value)) =>
            {
                found.push(Word::Whole(at))
            }
            _ => {}
        }
        at += 1;
    }
    found
}

/// Whether a string in `value`, at any depth, is a secret: a non-empty one that is not a
/// `${VAR}` reference under a key [`secret_name`] accepts, or a [`secret_value`] under any.
fn json_secret(key: &str, value: &Value) -> bool {
    match value {
        Value::String(text) => {
            (secret_name(key) && !text.is_empty() && reference(text).is_none())
                || secret_value(text)
        }
        Value::Array(items) => items.iter().any(|item| json_secret(key, item)),
        Value::Object(entries) => entries.iter().any(|(key, value)| json_secret(key, value)),
        _ => false,
    }
}

/// The words of `command` as a shell splits it, each without its quotes and with where
/// it is in `command`. Whitespace outside quotes ends a word; `'…'` keeps what it holds
/// as it is, and `"…"` too but for a `\` before `"`, `\`, `$` or a backtick, which
/// stands for that character. Outside quotes a `\` stands for the character after it,
/// a `\` before a newline joins the lines, and a `\` that ends `command` is kept.
pub(super) fn shell_words(command: &str) -> Vec<(String, std::ops::Range<usize>)> {
    let mut words = Vec::new();
    let mut chars = command.char_indices().peekable();
    loop {
        while chars
            .next_if(|&(at, c)| c.is_whitespace() || command[at..].starts_with("\\\n"))
            .is_some()
        {}
        let Some(&(start, _)) = chars.peek() else {
            return words;
        };
        let (mut word, mut quote) = (String::new(), None);
        while let Some((_, c)) = chars.next_if(|&(_, c)| quote.is_some() || !c.is_whitespace()) {
            match (quote, c) {
                (None, '\'' | '"') => quote = Some(c),
                (Some(open), c) if c == open => quote = None,
                (None, '\\') => match chars.next() {
                    Some((_, '\n')) => {}
                    Some((_, escaped)) => word.push(escaped),
                    None => word.push('\\'),
                },
                (Some('"'), '\\') => {
                    let escaped = chars.next_if(|(_, next)| matches!(next, '"' | '\\' | '$' | '`'));
                    word.push(escaped.map_or('\\', |(_, escaped)| escaped));
                }
                _ => word.push(c),
            }
        }
        let end = chars.peek().map_or(command.len(), |&(at, _)| at);
        words.push((word, start..end));
    }
}

/// `definition` with each secret's value as [`MASK`].
fn mask(definition: &Value, secrets: &[Secret]) -> Value {
    let mut out = definition.clone();
    for secret in secrets {
        match &secret.place {
            Place::Entry { table, name } => out[*table][name] = MASK.into(),
            Place::Arg(word) => {
                let (at, with) = word.masked();
                out["args"][at] = with.into();
            }
            Place::Command(hits) => {
                let mut command = out["command"].as_str().unwrap_or_default().to_owned();
                let spans = shell_words(&command);
                for hit in hits.iter().rev() {
                    let (at, with) = hit.masked();
                    command.replace_range(spans[at].1.clone(), &with);
                }
                out["command"] = command.into();
            }
            Place::Url => {
                let url = url_secrets(out["url"].as_str().unwrap_or_default(), Some(MASK));
                out["url"] = url.into();
            }
        }
    }
    out
}

/// `definition` without the secrets `drop` picks, and without a table or argument
/// list that leaves empty. A command loses each secret's words and the whitespace that
/// parts them from the rest, and keeps every other byte.
fn leave_out(definition: &Value, secrets: &[Secret], drop: impl Fn(&Secret) -> bool) -> Value {
    let mut out = definition.clone();
    let mut gone = Vec::new();
    for secret in secrets.iter().filter(|secret| drop(secret)) {
        match &secret.place {
            Place::Entry { table, name } => {
                if let Some(entries) = out.get_mut(*table).and_then(Value::as_object_mut) {
                    entries.remove(name);
                }
            }
            Place::Arg(word) => gone.extend(word.span()),
            Place::Command(hits) => {
                let mut command = out["command"].as_str().unwrap_or_default().to_owned();
                let spans = shell_words(&command);
                for hit in hits.iter().rev() {
                    let taken = hit.span();
                    let start = taken.start.checked_sub(1).map_or(0, |at| spans[at].1.end);
                    command.replace_range(start..spans[taken.end - 1].1.end, "");
                    if start == 0 {
                        command = command.trim_start().to_owned();
                    }
                }
                out["command"] = command.into();
            }
            Place::Url => {
                let url = url_secrets(out["url"].as_str().unwrap_or_default(), None);
                out["url"] = url.into();
            }
        }
    }
    if let Some(args) = out.get_mut("args").and_then(Value::as_array_mut) {
        let mut at = 0;
        args.retain(|_| {
            at += 1;
            !gone.contains(&(at - 1))
        });
    }
    prune(&mut out);
    out
}

/// Takes out of `definition` each table and list left empty.
fn prune(definition: &mut Value) {
    if let Some(object) = definition.as_object_mut() {
        object.retain(|_, value| match value {
            Value::Object(entries) => !entries.is_empty(),
            Value::Array(list) => !list.is_empty(),
            _ => true,
        });
    }
}

/// `definition` as two equal servers have it: without Codex's `enabled`, which is a
/// switch, and with numbers as floats, since `codex mcp` rewrites `20` as `20.0`.
fn normal(definition: Value) -> Value {
    fn numbers(value: Value) -> Value {
        match value {
            Value::Number(number) => number.as_f64().map_or(Value::Null, Value::from),
            Value::Array(list) => list.into_iter().map(numbers).collect(),
            Value::Object(object) => object.into_iter().map(|(k, v)| (k, numbers(v))).collect(),
            leaf => leaf,
        }
    }
    let mut definition = numbers(definition);
    if let Some(object) = definition.as_object_mut() {
        object.remove("enabled");
    }
    definition
}

/// Every file under `folder`, as paths relative to it, sorted. A link is listed, not
/// followed. Whether any is a relative link that leads out of `folder`, which a copy
/// would leave pointing elsewhere.
fn list_files(folder: &Path, at: &Path, files: &mut Vec<String>) -> bool {
    let mut escapes = false;
    let mut entries: Vec<_> = fs::read_dir(folder.join(at))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .collect();
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = at.join(entry.file_name());
        let kind = entry.file_type().ok();
        if kind.is_some_and(|kind| kind.is_dir()) {
            escapes |= list_files(folder, &path, files);
            continue;
        }
        if kind.is_some_and(|kind| kind.is_symlink())
            && let Ok(target) = fs::read_link(entry.path())
            && target.is_relative()
        {
            let mut depth = at.components().count() as isize;
            for part in target.components() {
                match part {
                    std::path::Component::ParentDir => depth -= 1,
                    std::path::Component::Normal(_) => depth += 1,
                    _ => {}
                }
                escapes |= depth < 0;
            }
        }
        files.push(path.display().to_string());
    }
    escapes
}

#[cfg(test)]
#[path = "copy_tests.rs"]
mod tests;
