//! Which session each conversation view of a project holds.

use crate::composition::ViewId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// The conversation views that hold a session, each a session tab. A view not listed
/// follows the selected session, as every view did before tabs held sessions. A
/// session is held by at most one view.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(from = "Vec<Held>", into = "Vec<Held>")]
pub struct SessionTabs(BTreeMap<ViewId, u64>);

#[derive(Deserialize, Serialize)]
struct Held {
    view: ViewId,
    session: u64,
}

impl From<Vec<Held>> for SessionTabs {
    /// The first view listed for a session keeps it.
    fn from(held: Vec<Held>) -> Self {
        let mut tabs = Self::default();
        for Held { view, session } in held {
            if tabs.view_of(session).is_none() && tabs.get(view).is_none() {
                tabs.0.insert(view, session);
            }
        }
        tabs
    }
}

impl From<SessionTabs> for Vec<Held> {
    fn from(tabs: SessionTabs) -> Self {
        tabs.0
            .into_iter()
            .map(|(view, session)| Held { view, session })
            .collect()
    }
}

impl SessionTabs {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, view: ViewId) -> Option<u64> {
        self.0.get(&view).copied()
    }

    pub fn view_of(&self, session: u64) -> Option<ViewId> {
        self.0
            .iter()
            .find_map(|(&view, &held)| (held == session).then_some(view))
    }

    pub fn iter(&self) -> impl Iterator<Item = (ViewId, u64)> + '_ {
        self.0.iter().map(|(&view, &session)| (view, session))
    }

    /// `view` holds `session`, which leaves any other view that held it.
    pub fn hold(&mut self, view: ViewId, session: u64) {
        self.0.retain(|_, &mut held| held != session);
        self.0.insert(view, session);
    }

    /// `view` follows the selected session again.
    pub fn release(&mut self, view: ViewId) {
        self.0.remove(&view);
    }

    pub fn retain(&mut self, mut keep: impl FnMut(ViewId, u64) -> bool) {
        self.0.retain(|&view, &mut session| keep(view, session));
    }
}

/// What a project saved for its session tabs, kept as is when this version cannot
/// read it.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub(crate) enum SavedTabs {
    Supported(SessionTabs),
    Unsupported(Value),
}

impl SavedTabs {
    /// The tabs to draw: the saved ones, or none held when they cannot be read.
    pub(crate) fn shown(&self) -> SessionTabs {
        match self {
            Self::Supported(tabs) => tabs.clone(),
            Self::Unsupported(_) => SessionTabs::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_session_is_held_by_one_view_only() {
        let mut tabs = SessionTabs::default();
        tabs.hold(ViewId(0), 1);
        tabs.hold(ViewId(3), 2);
        tabs.hold(ViewId(4), 1);
        assert_eq!(tabs.get(ViewId(0)), None);
        assert_eq!(tabs.view_of(1), Some(ViewId(4)));
        assert_eq!(tabs.view_of(2), Some(ViewId(3)));
        tabs.hold(ViewId(3), 5);
        assert_eq!(tabs.view_of(2), None);
        assert_eq!(tabs.get(ViewId(3)), Some(5));
        tabs.release(ViewId(3));
        assert_eq!(tabs.iter().collect::<Vec<_>>(), [(ViewId(4), 1)]);

        let read: SessionTabs = serde_json::from_value(json!([
            {"view": 0, "session": 1},
            {"view": 3, "session": 1},
            {"view": 0, "session": 2},
            {"view": 5, "session": 2, "pinned": true},
        ]))
        .unwrap();
        assert_eq!(
            read.iter().collect::<Vec<_>>(),
            [(ViewId(0), 1), (ViewId(5), 2)]
        );
    }

    #[test]
    fn tabs_round_trip_and_unreadable_ones_are_kept() {
        let mut tabs = SessionTabs::default();
        tabs.hold(ViewId(3), 7);
        tabs.hold(ViewId(0), 2);
        let saved = serde_json::to_value(&tabs).unwrap();
        assert_eq!(
            saved,
            json!([{"view": 0, "session": 2}, {"view": 3, "session": 7}])
        );
        assert_eq!(serde_json::from_value::<SessionTabs>(saved).unwrap(), tabs);

        for unreadable in [json!({"view": 0}), json!("tabs"), json!([{"view": -1}])] {
            let saved: SavedTabs = serde_json::from_value(unreadable.clone()).unwrap();
            assert!(saved.shown().is_empty(), "{unreadable}");
            assert_eq!(serde_json::to_value(&saved).unwrap(), unreadable);
        }
    }
}
