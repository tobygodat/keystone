//! Durable delivery records: the result of a session an agent launched, on its way to
//! the session that launched it.

use crate::{Applied, Store, StoreResult, Workspace, invalid};
use serde::{Deserialize, Serialize};

/// One launched session's result and where it stands. Saved in its own projection,
/// with a `delivery.upserted` event for every change.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Delivery {
    /// The session an agent launched.
    child: u64,
    /// The session whose agent launched it, which receives its result.
    parent: u64,
    #[serde(flatten)]
    state: DeliveryState,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DeliveryState {
    /// The launched session is working on its task.
    Working,
    /// Its turn ended. `message` waits for the parent to be idle.
    Ready { message: String },
    /// `message` started a turn of the parent. Saved before the message goes, so a
    /// result is never sent twice.
    Delivered {
        #[serde(default)]
        message: String,
    },
    /// The parent's own `session_wait` saw the turn end, so no turn was started for it.
    Observed,
    /// The launch failed before its task was sent.
    Unsent,
}

impl Delivery {
    /// The delivery of `child`, which an agent in `parent` just launched.
    pub(crate) fn launched(child: u64, parent: u64) -> Self {
        Self {
            child,
            parent,
            state: DeliveryState::Working,
        }
    }

    pub fn child(&self) -> u64 {
        self.child
    }

    pub fn parent(&self) -> u64 {
        self.parent
    }

    pub fn state(&self) -> &DeliveryState {
        &self.state
    }
}

impl Workspace {
    /// Every launched session's delivery, in launch order.
    pub fn deliveries(&self) -> &[Delivery] {
        &self.deliveries
    }

    /// The delivery of the launched session `child`.
    pub fn delivery(&self, child: u64) -> Option<&Delivery> {
        self.deliveries
            .iter()
            .find(|delivery| delivery.child == child)
    }
}

impl Store {
    /// Moves the delivery of `child` to `state`.
    pub fn set_delivery(&mut self, child: u64, state: DeliveryState) -> StoreResult<Applied<()>> {
        let mut next = self.workspace.clone();
        let delivery = next
            .deliveries
            .iter_mut()
            .find(|delivery| delivery.child == child)
            .ok_or_else(|| invalid(format!("session {child} has no delivery")))?;
        if delivery.state == state {
            return Ok(self.unchanged());
        }
        delivery.state = state;
        Ok(self.commit(next, self.next_id))
    }
}

/// Every delivery names two sessions that exist, and each launched session has one.
pub(crate) fn validate(workspace: &Workspace) -> StoreResult<()> {
    let exists = |id: u64| workspace.sessions.iter().any(|s| s.id == id);
    let mut children = std::collections::HashSet::new();
    for delivery in &workspace.deliveries {
        if !exists(delivery.child) || !exists(delivery.parent) || !children.insert(delivery.child) {
            return Err(invalid("invalid or duplicate delivery").into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Database, Provider, WorkingDirectory};

    fn directory() -> WorkingDirectory {
        WorkingDirectory::new(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn a_delivery_is_saved_with_its_session_and_each_change_is_an_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.json");
        let mut store = Store::open(&path).unwrap();
        let parent = store
            .create(Provider::Claude, &directory(), 1)
            .unwrap()
            .value;
        assert!(
            store
                .create_unselected(Provider::Codex, &directory(), 2, 99)
                .is_err(),
            "a launch names a parent that exists"
        );
        let child = store
            .create_unselected(Provider::Codex, &directory(), 2, parent)
            .unwrap()
            .value;
        let ready = DeliveryState::Ready {
            message: "Result from session 2".into(),
        };
        store.set_delivery(child, ready.clone()).unwrap();
        let writes = store.writes();
        store.set_delivery(child, ready.clone()).unwrap();
        assert_eq!(store.writes(), writes, "an unchanged state writes nothing");
        assert!(store.set_delivery(parent, DeliveryState::Observed).is_err());

        let reopened = Store::open(&path).unwrap();
        let saved = reopened.workspace().delivery(child).unwrap();
        assert_eq!((saved.parent(), saved.state()), (parent, &ready));
        assert_eq!(reopened.workspace().deliveries().len(), 1);

        store
            .set_delivery(
                child,
                DeliveryState::Delivered {
                    message: "done".into(),
                },
            )
            .unwrap();
        let events = Database::open(&path)
            .unwrap()
            .events_after(0, 1000)
            .unwrap();
        let states: Vec<String> = events
            .iter()
            .filter(|event| event.kind == "delivery.upserted")
            .map(|event| {
                assert_eq!(event.entity_id, child.to_string());
                serde_json::from_str::<serde_json::Value>(&event.payload).unwrap()["state"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(states, ["working", "ready", "delivered"]);
        let workspace: serde_json::Value = serde_json::from_str(
            &events
                .iter()
                .rev()
                .find(|event| event.kind == "workspace.upserted")
                .unwrap()
                .payload,
        )
        .unwrap();
        assert!(
            workspace["workspace"].get("deliveries").is_none(),
            "deliveries live in their own projection"
        );
    }

    /// A schema 3 database (before prompts) and a schema 4 one with a saved prompt
    /// both gain deliveries and keep their sessions and prompts.
    #[test]
    fn schema_3_and_prompts_schema_4_databases_gain_deliveries() {
        for (version, removed) in [
            (
                3,
                "DROP TABLE notes; DROP TABLE deliveries; DROP TABLE prompts;",
            ),
            (4, "DROP TABLE notes; DROP TABLE deliveries;"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("workspace.json");
            let parent = {
                let mut store = Store::open(&path).unwrap();
                if version == 4 {
                    store
                        .add_prompt(crate::PromptFields {
                            title: "Kept".into(),
                            body: "kept body".into(),
                            shortcut: String::new(),
                            project: None,
                        })
                        .unwrap();
                }
                store
                    .create(Provider::Claude, &directory(), 1)
                    .unwrap()
                    .value
            };
            rusqlite::Connection::open(path.with_extension("sqlite3"))
                .unwrap()
                .execute_batch(&format!("{removed} PRAGMA user_version={version};"))
                .unwrap();
            let mut store = Store::open(&path).unwrap();
            assert_eq!(store.workspace().sessions().len(), 1, "version {version}");
            assert!(store.workspace().deliveries().is_empty());
            let prompts: Vec<&str> = store
                .workspace()
                .prompts()
                .iter()
                .map(|p| p.title())
                .collect();
            assert_eq!(prompts, if version == 4 { vec!["Kept"] } else { vec![] });
            let child = store
                .create_unselected(Provider::Codex, &directory(), 2, parent)
                .unwrap()
                .value;
            let reopened = Store::open(&path).unwrap();
            assert_eq!(
                reopened.workspace().delivery(child).unwrap().parent(),
                parent
            );
            assert_eq!(
                reopened.workspace().prompts().len(),
                usize::from(version == 4)
            );
            let schema: u32 = rusqlite::Connection::open(path.with_extension("sqlite3"))
                .unwrap()
                .query_one("PRAGMA user_version", [], |row| row.get(0))
                .unwrap();
            assert_eq!(schema, crate::database::SCHEMA_VERSION);
            assert_eq!(schema, 6);
        }
    }

    #[test]
    fn a_delivery_naming_a_missing_session_is_refused() {
        let mut workspace = Workspace::default();
        workspace.deliveries.push(Delivery {
            child: 2,
            parent: 1,
            state: DeliveryState::Working,
        });
        assert!(validate(&workspace).is_err());
    }
}
