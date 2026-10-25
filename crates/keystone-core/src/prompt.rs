use crate::{ProjectId, invalid, one_line};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::io;

/// A prompt the user saved to insert into the composer: in every project, or only in
/// windows that have one project open. Inserting fills the composer; it never sends.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Prompt {
    /// From the same counter as session IDs, so no ID is ever reused.
    id: u64,
    title: String,
    body: String,
    /// What follows `/` to find it in the composer's menu, without the `/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shortcut: Option<String>,
    /// The project it belongs to, by [`ProjectId`], or `None` for every project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    #[serde(flatten)]
    unknown: Map<String, Value>,
}

/// What the user typed for a prompt, before [`PromptFields::check`] cleans it.
#[derive(Clone, Debug, Default)]
pub struct PromptFields {
    pub title: String,
    pub body: String,
    /// With or without its leading `/`; blank for none.
    pub shortcut: String,
    pub project: Option<ProjectId>,
}

impl Prompt {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn body(&self) -> &str {
        &self.body
    }

    pub fn shortcut(&self) -> Option<&str> {
        self.shortcut.as_deref()
    }

    pub fn project(&self) -> Option<ProjectId> {
        self.project.as_deref().map(ProjectId::from_saved)
    }

    /// Whether `other` has the same title, text, shortcut and project.
    pub fn same_content(&self, other: &Prompt) -> bool {
        (&self.title, &self.body, &self.shortcut, &self.project)
            == (&other.title, &other.body, &other.shortcut, &other.project)
    }

    /// Whether it is offered in a window with `project` open: always when global,
    /// otherwise only in its own project.
    pub fn shown_in(&self, project: Option<&ProjectId>) -> bool {
        match &self.project {
            None => true,
            Some(mine) => project.is_some_and(|project| project.as_saved() == mine),
        }
    }

    pub(crate) fn new(id: u64, fields: Checked) -> Self {
        let mut prompt = Self {
            id,
            title: String::new(),
            body: String::new(),
            shortcut: None,
            project: None,
            unknown: Map::new(),
        };
        prompt.apply(fields);
        prompt
    }

    pub(crate) fn apply(&mut self, fields: Checked) {
        self.title = fields.title;
        self.body = fields.body;
        self.shortcut = fields.shortcut;
        self.project = fields.project.map(|p| p.as_saved().to_owned());
    }

    /// Whether this prompt and one in `project` could both be offered in one window.
    fn overlaps(&self, project: Option<&str>) -> bool {
        match (self.project.as_deref(), project) {
            (Some(mine), Some(theirs)) => mine == theirs,
            _ => true,
        }
    }
}

/// Fields [`PromptFields::check`] accepted.
pub(crate) struct Checked {
    title: String,
    body: String,
    shortcut: Option<String>,
    project: Option<ProjectId>,
}

/// Whether `shortcut` can follow `/` as one word.
fn valid_shortcut(shortcut: &str) -> bool {
    shortcut
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
}

impl PromptFields {
    /// The fields as they would be saved, or why they cannot be: a blank title or
    /// body, a shortcut that is not one word, or one another prompt shown in the same
    /// windows already uses. `except` is the prompt being edited.
    pub(crate) fn check(self, prompts: &[Prompt], except: Option<u64>) -> io::Result<Checked> {
        let title = one_line(&self.title).ok_or_else(|| invalid("Add a title."))?;
        if self.body.trim().is_empty() {
            return Err(invalid("Add the text."));
        }
        let shortcut = self.shortcut.trim();
        let shortcut = shortcut.strip_prefix('/').unwrap_or(shortcut);
        let shortcut = (!shortcut.is_empty()).then(|| shortcut.to_owned());
        if let Some(shortcut) = &shortcut {
            if !valid_shortcut(shortcut) {
                return Err(invalid("Shortcut: A–Z, a–z, 0–9, - _ : . only."));
            }
            let project = self.project.as_ref().map(ProjectId::as_saved);
            if let Some(other) = prompts.iter().find(|other| {
                Some(other.id) != except
                    && other
                        .shortcut
                        .as_deref()
                        .is_some_and(|s| s.eq_ignore_ascii_case(shortcut))
                    && other.overlaps(project)
            }) {
                return Err(invalid(format!(
                    "/{shortcut} already inserts \u{201c}{}\u{201d}",
                    other.title
                )));
            }
        }
        Ok(Checked {
            title,
            body: self.body,
            shortcut,
            project: self.project,
        })
    }
}
