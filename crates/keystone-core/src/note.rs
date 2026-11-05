//! Notes: one free-text note per project, kept in Keystone's database rather than in
//! the project's folder.

use crate::{Applied, ProjectId, Store, StoreResult, Workspace, invalid};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashSet;

/// A project's note. Saved in its own projection, with a `note.upserted` event for
/// every change and `note.removed` when it is emptied.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Note {
    /// The project's key, its canonical folder.
    project: String,
    text: String,
    #[serde(flatten)]
    unknown: Map<String, Value>,
}

impl Note {
    pub fn project(&self) -> ProjectId {
        ProjectId::from_saved(&self.project)
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

impl Workspace {
    /// Every project's note, in the order they were first written.
    pub fn notes(&self) -> &[Note] {
        &self.notes
    }

    /// The note of `project`, empty when it has none.
    pub fn note(&self, project: &ProjectId) -> &str {
        self.notes
            .iter()
            .find(|note| note.project == project.as_saved())
            .map_or("", |note| &note.text)
    }
}

impl Store {
    /// Saves `text` as the note of `project`. An empty note is removed.
    pub fn set_note(&mut self, project: &ProjectId, text: String) -> StoreResult<Applied<()>> {
        let mut next = self.workspace.clone();
        let position = next
            .notes
            .iter()
            .position(|note| note.project == project.as_saved());
        match (position, text.is_empty()) {
            (Some(position), true) => {
                next.notes.remove(position);
            }
            (Some(position), false) => next.notes[position].text = text,
            (None, true) => {}
            (None, false) => next.notes.push(Note {
                project: project.as_saved().to_owned(),
                text,
                unknown: Map::new(),
            }),
        }
        if next == self.workspace {
            return Ok(self.unchanged());
        }
        Ok(self.commit(next, self.next_id))
    }
}

/// Every note names one absolute project folder, and no folder has two.
pub(crate) fn validate(workspace: &Workspace) -> StoreResult<()> {
    let mut projects = HashSet::new();
    for note in &workspace.notes {
        if !note.project().path().is_absolute() || !projects.insert(&note.project) {
            return Err(invalid("invalid or duplicate note").into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Database, Provider, WorkingDirectory};

    fn project(path: &std::path::Path) -> ProjectId {
        ProjectId::new(path).unwrap()
    }

    #[test]
    fn each_project_keeps_its_own_note_across_reopening_and_each_change_is_an_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let (first, second) = (dir.path().join("first"), dir.path().join("second"));
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let (first, second) = (project(&first), project(&second));
        let mut store = Store::open(&path).unwrap();
        assert_eq!(store.workspace().note(&first), "");
        store.set_note(&first, "Ship notes\nthen".into()).unwrap();
        store.set_note(&second, "Other project".into()).unwrap();
        let writes = store.writes();
        store.set_note(&first, "Ship notes\nthen".into()).unwrap();
        store
            .set_note(&ProjectId::from_saved("/nowhere"), String::new())
            .unwrap();
        assert_eq!(store.writes(), writes, "an unchanged note writes nothing");

        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.workspace().note(&first), "Ship notes\nthen");
        assert_eq!(reopened.workspace().note(&second), "Other project");

        store.set_note(&first, "Ship notes".into()).unwrap();
        store.set_note(&second, String::new()).unwrap();
        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.workspace().note(&first), "Ship notes");
        assert_eq!(reopened.workspace().note(&second), "");
        assert_eq!(
            reopened.workspace().notes().len(),
            1,
            "an emptied note is removed"
        );

        let events = Database::open(&path)
            .unwrap()
            .events_after(0, 1000)
            .unwrap();
        let notes: Vec<(&str, &str, String)> = events
            .iter()
            .filter(|event| event.kind.starts_with("note."))
            .map(|event| {
                let payload: Value = serde_json::from_str(&event.payload).unwrap();
                (
                    event.kind.as_str(),
                    event.entity_id.rsplit('/').next().unwrap(),
                    payload["text"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        assert_eq!(
            notes,
            [
                ("note.upserted", "first", "Ship notes\nthen".to_owned()),
                ("note.upserted", "second", "Other project".to_owned()),
                ("note.upserted", "first", "Ship notes".to_owned()),
                ("note.removed", "second", "Other project".to_owned()),
            ]
        );
        let workspace: Value = serde_json::from_str(
            &events
                .iter()
                .rev()
                .find(|event| event.kind == "workspace.upserted")
                .unwrap()
                .payload,
        )
        .unwrap();
        assert!(
            workspace["workspace"].get("notes").is_none(),
            "notes live in their own projection"
        );
    }

    #[test]
    fn emptying_a_note_records_only_its_removal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let projects: Vec<ProjectId> = ["a", "b", "c"]
            .iter()
            .map(|name| {
                std::fs::create_dir(dir.path().join(name)).unwrap();
                project(&dir.path().join(name))
            })
            .collect();
        let mut store = Store::open(&path).unwrap();
        for (project, text) in projects.iter().zip(["A", "B", "C"]) {
            store.set_note(project, text.into()).unwrap();
        }
        let database = Database::open(&path).unwrap();
        let before = database.events_after(0, 1000).unwrap().len();
        store.set_note(&projects[0], String::new()).unwrap();
        let kinds: Vec<String> = database
            .events_after(0, 1000)
            .unwrap()
            .into_iter()
            .skip(before)
            .filter(|event| event.kind.starts_with("note."))
            .map(|event| event.kind)
            .collect();
        assert_eq!(
            kinds,
            ["note.removed"],
            "the notes that moved up keep their text"
        );
        let reopened = Store::open(&path).unwrap();
        let texts: Vec<&str> = reopened
            .workspace()
            .notes()
            .iter()
            .map(Note::text)
            .collect();
        assert_eq!(texts, ["B", "C"]);
        assert_eq!(reopened.workspace().note(&projects[2]), "C");
    }

    /// A schema 5 database, from before notes, gains an empty note list and keeps its
    /// sessions.
    #[test]
    fn a_deliveries_schema_5_database_gains_notes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let folder = WorkingDirectory::new(dir.path()).unwrap();
        Store::open(&path)
            .unwrap()
            .create(Provider::Claude, &folder, 1)
            .unwrap();
        rusqlite::Connection::open(path.with_extension("sqlite3"))
            .unwrap()
            .execute_batch("DROP TABLE notes; PRAGMA user_version=5;")
            .unwrap();
        let mut store = Store::open(&path).unwrap();
        assert_eq!(store.workspace().sessions().len(), 1);
        assert!(store.workspace().notes().is_empty());
        store
            .set_note(&project(dir.path()), "After the upgrade".into())
            .unwrap();
        let reopened = Store::open(&path).unwrap();
        assert_eq!(
            reopened.workspace().note(&project(dir.path())),
            "After the upgrade"
        );
        let schema: u32 = rusqlite::Connection::open(path.with_extension("sqlite3"))
            .unwrap()
            .query_one("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(schema, crate::database::SCHEMA_VERSION);
        assert_eq!(schema, 6);
    }

    #[test]
    fn a_duplicate_or_relative_note_is_refused() {
        let note = |project: &str| Note {
            project: project.into(),
            text: "x".into(),
            unknown: Map::new(),
        };
        let mut workspace = Workspace::default();
        workspace.notes = vec![note("/a"), note("/a")];
        assert!(validate(&workspace).is_err());
        workspace.notes = vec![note("relative")];
        assert!(validate(&workspace).is_err());
        workspace.notes = vec![note("/a"), note("/b")];
        assert!(validate(&workspace).is_ok());
    }
}
