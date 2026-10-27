use crate::layout::{SidebarLayout, SidebarSide};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::num::NonZeroU16;

/// One occurrence of a component in a composition.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ViewId(pub u32);

impl ViewId {
    pub const MAIN: ViewId = ViewId(0);
}

const SESSION_LIST: ViewId = ViewId(1);
const COMPOSER: ViewId = ViewId(2);

const VERSION: u32 = 1;
const MAX_DEPTH: usize = 32;
const MAX_NODES: usize = 256;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Component {
    SessionList,
    Conversation,
    Composer,
    Files,
    /// The repository's changed files, with staging and commit.
    Changes,
    Notes,
    /// The session's pull request, its checks and its reviews.
    PullRequest,
}

impl Component {
    pub const ALL: [Component; 7] = [
        Component::SessionList,
        Component::Conversation,
        Component::Composer,
        Component::Files,
        Component::Changes,
        Component::Notes,
        Component::PullRequest,
    ];

    pub fn allows(self, count: usize) -> bool {
        match self {
            Self::SessionList | Self::Files | Self::Changes | Self::Notes | Self::PullRequest => {
                count <= 1
            }
            Self::Conversation => count >= 1,
            Self::Composer => count == 1,
        }
    }

    /// Whether occurrences of this component can share an area as tabs.
    pub fn is_surface(self) -> bool {
        matches!(
            self,
            Self::Conversation | Self::Files | Self::Changes | Self::Notes | Self::PullRequest
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    Row,
    Column,
}

/// A child's preferred extent along its split's axis: logical pixels, its content's
/// own extent, or a share of what the other children leave.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Size {
    Px(NonZeroU16),
    Auto,
    Fill(NonZeroU16),
}

impl Size {
    const FILL: Size = Size::Fill(NonZeroU16::MIN);
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum Node {
    View {
        id: ViewId,
    },
    Split {
        axis: Axis,
        children: Vec<Child>,
    },
    /// Surface occurrences sharing one area, showing `views[selected]`.
    Tabs {
        selected: usize,
        views: Vec<ViewId>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Child {
    pub node: Node,
    pub size: Size,
}

/// Indices into nested split children, starting at the root.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct NodePath(pub Vec<usize>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Place {
    Beside {
        target: ViewId,
        axis: Axis,
        after: bool,
    },
    TabWith(ViewId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Edit {
    Move {
        view: ViewId,
        to: Place,
    },
    Resize {
        split: NodePath,
        sizes: Vec<Size>,
    },
    Open {
        view: ViewId,
        component: Component,
        at: Place,
        size: Size,
    },
    Close(ViewId),
    SelectTab(ViewId),
    /// Moves a tabbed view to `index` in its tab group. The shown tab stays shown.
    Reorder {
        view: ViewId,
        index: usize,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Problem {
    Version(u32),
    DuplicateView(ViewId),
    UnknownView(ViewId),
    PlacedTwice(ViewId),
    Unplaced(ViewId),
    Count(Component),
    TooDeep,
    TooLarge,
    SingleChild,
    TabNotSurface(ViewId),
    TabOutOfRange,
    NotTabbed(ViewId),
    NoSplit,
    SizeCount,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Version(version) => write!(f, "unsupported composition version {version}"),
            Self::DuplicateView(view) => write!(f, "view {} is declared twice", view.0),
            Self::UnknownView(view) => write!(f, "view {} is not declared", view.0),
            Self::PlacedTwice(view) => write!(f, "view {} is placed twice", view.0),
            Self::Unplaced(view) => write!(f, "view {} is not placed", view.0),
            Self::Count(component) => write!(f, "wrong number of {component:?} views"),
            Self::TooDeep => write!(f, "deeper than {MAX_DEPTH} levels"),
            Self::TooLarge => write!(f, "more than {MAX_NODES} nodes"),
            Self::SingleChild => f.write_str("a split or tab group has fewer than two children"),
            Self::TabNotSurface(view) => {
                write!(
                    f,
                    "view {} is tabbed but is not a conversation, files or changes view",
                    view.0
                )
            }
            Self::TabOutOfRange => f.write_str("the selected tab does not exist"),
            Self::NotTabbed(view) => write!(f, "view {} is not in a tab group", view.0),
            Self::NoSplit => f.write_str("no split at that path"),
            Self::SizeCount => f.write_str("sizes do not match the split's children"),
        }
    }
}

impl Error for Problem {}

/// A valid arrangement. Every way to build one upholds every rule, so a held value
/// never needs checking again.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "Document", into = "Document")]
pub struct Composition {
    root: Node,
    views: BTreeMap<ViewId, Component>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    views: Vec<Occurrence>,
    root: Node,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Occurrence {
    id: ViewId,
    component: Component,
}

impl TryFrom<Document> for Composition {
    type Error = Problem;

    fn try_from(document: Document) -> Result<Self, Problem> {
        if document.version != VERSION {
            return Err(Problem::Version(document.version));
        }
        if document.views.len() > MAX_NODES {
            return Err(Problem::TooLarge);
        }
        let mut views = BTreeMap::new();
        for occurrence in document.views {
            if views.insert(occurrence.id, occurrence.component).is_some() {
                return Err(Problem::DuplicateView(occurrence.id));
            }
        }
        Self::checked(document.root, views)
    }
}

impl From<Composition> for Document {
    fn from(composition: Composition) -> Self {
        Self {
            version: VERSION,
            views: composition
                .views
                .into_iter()
                .map(|(id, component)| Occurrence { id, component })
                .collect(),
            root: composition.root,
        }
    }
}

impl Default for Composition {
    fn default() -> Self {
        Self::from_sidebar(SidebarLayout::default())
    }
}

impl Composition {
    /// Today's arrangement: the session list at the sidebar's side and width, and one
    /// conversation above the composer.
    pub fn from_sidebar(sidebar: SidebarLayout) -> Self {
        let list = Child {
            node: Node::View { id: SESSION_LIST },
            size: Size::Px(NonZeroU16::new(sidebar.width.px()).expect("sidebar width")),
        };
        let main = Child {
            node: Node::Split {
                axis: Axis::Column,
                children: vec![
                    Child {
                        node: Node::View { id: ViewId::MAIN },
                        size: Size::FILL,
                    },
                    Child {
                        node: Node::View { id: COMPOSER },
                        size: Size::Auto,
                    },
                ],
            },
            size: Size::FILL,
        };
        let children = match sidebar.side {
            SidebarSide::Left => vec![list, main],
            SidebarSide::Right => vec![main, list],
        };
        Self {
            root: Node::Split {
                axis: Axis::Row,
                children,
            },
            views: BTreeMap::from([
                (ViewId::MAIN, Component::Conversation),
                (SESSION_LIST, Component::SessionList),
                (COMPOSER, Component::Composer),
            ]),
        }
    }

    pub fn root(&self) -> &Node {
        &self.root
    }

    pub fn component(&self, view: ViewId) -> Option<Component> {
        self.views.get(&view).copied()
    }

    pub fn views(&self) -> impl Iterator<Item = (ViewId, Component)> + '_ {
        self.views
            .iter()
            .map(|(&view, &component)| (view, component))
    }

    /// The composition with `edit` applied. A refused edit returns why and changes
    /// nothing.
    pub fn edit(&self, edit: Edit) -> Result<Self, Problem> {
        let mut root = self.root.clone();
        let mut views = self.views.clone();
        match edit {
            Edit::Move { view, to } => {
                if !self.views.contains_key(&view) {
                    return Err(Problem::UnknownView(view));
                }
                if to.target() == view {
                    return Ok(self.clone());
                }
                if !reorder(&mut root, view, to) {
                    let size = remove(&mut root, view);
                    insert(&mut root, view, to, size.unwrap_or(Size::FILL))?;
                }
            }
            Edit::Resize { split, sizes } => {
                let Some(Node::Split { children, .. }) = node_mut(&mut root, &split.0) else {
                    return Err(Problem::NoSplit);
                };
                if children.len() != sizes.len() {
                    return Err(Problem::SizeCount);
                }
                for (child, size) in children.iter_mut().zip(sizes) {
                    child.size = size;
                }
            }
            Edit::Open {
                view,
                component,
                at,
                size,
            } => {
                if views.insert(view, component).is_some() {
                    return Err(Problem::DuplicateView(view));
                }
                insert(&mut root, view, at, size)?;
            }
            Edit::Close(view) => {
                views.remove(&view).ok_or(Problem::UnknownView(view))?;
                remove(&mut root, view);
            }
            Edit::SelectTab(view) => {
                let path = slot(&root, view).ok_or(Problem::UnknownView(view))?;
                let Some(Node::Tabs { selected, views }) = node_mut(&mut root, &path) else {
                    return Err(Problem::NotTabbed(view));
                };
                *selected = views.iter().position(|&v| v == view).expect("tabbed");
            }
            Edit::Reorder { view, index } => {
                let path = slot(&root, view).ok_or(Problem::UnknownView(view))?;
                let Some(Node::Tabs { selected, views }) = node_mut(&mut root, &path) else {
                    return Err(Problem::NotTabbed(view));
                };
                let shown = views[*selected];
                let from = views.iter().position(|&v| v == view).expect("tabbed");
                views.remove(from);
                views.insert(index.min(views.len()), view);
                *selected = views.iter().position(|&v| v == shown).expect("tabbed");
            }
        }
        Self::checked(root, views)
    }

    fn checked(root: Node, views: BTreeMap<ViewId, Component>) -> Result<Self, Problem> {
        let mut placed = BTreeSet::new();
        let mut nodes = 0;
        check(&root, 1, &views, &mut placed, &mut nodes)?;
        if let Some(&view) = views.keys().find(|view| !placed.contains(view)) {
            return Err(Problem::Unplaced(view));
        }
        for component in Component::ALL {
            if !component.allows(views.values().filter(|&&c| c == component).count()) {
                return Err(Problem::Count(component));
            }
        }
        Ok(Self { root, views })
    }
}

fn check(
    node: &Node,
    depth: usize,
    views: &BTreeMap<ViewId, Component>,
    placed: &mut BTreeSet<ViewId>,
    nodes: &mut usize,
) -> Result<(), Problem> {
    if depth > MAX_DEPTH {
        return Err(Problem::TooDeep);
    }
    let mut count = |placed: &mut BTreeSet<ViewId>, view: Option<ViewId>| {
        *nodes += 1;
        if *nodes > MAX_NODES {
            return Err(Problem::TooLarge);
        }
        let Some(view) = view else { return Ok(()) };
        if !views.contains_key(&view) {
            return Err(Problem::UnknownView(view));
        }
        if !placed.insert(view) {
            return Err(Problem::PlacedTwice(view));
        }
        Ok(())
    };
    match node {
        Node::View { id } => count(placed, Some(*id)),
        Node::Split { children, .. } => {
            count(placed, None)?;
            if children.len() < 2 {
                return Err(Problem::SingleChild);
            }
            children
                .iter()
                .try_for_each(|child| check(&child.node, depth + 1, views, placed, nodes))
        }
        Node::Tabs {
            selected,
            views: tabbed,
        } => {
            count(placed, None)?;
            if tabbed.len() < 2 {
                return Err(Problem::SingleChild);
            }
            if *selected >= tabbed.len() {
                return Err(Problem::TabOutOfRange);
            }
            for &view in tabbed {
                count(placed, Some(view))?;
                if !views[&view].is_surface() {
                    return Err(Problem::TabNotSurface(view));
                }
            }
            Ok(())
        }
    }
}

impl Place {
    fn target(self) -> ViewId {
        match self {
            Self::Beside { target, .. } | Self::TabWith(target) => target,
        }
    }
}

fn slot(node: &Node, view: ViewId) -> Option<Vec<usize>> {
    match node {
        Node::View { id } => (*id == view).then(Vec::new),
        Node::Tabs { views, .. } => views.contains(&view).then(Vec::new),
        Node::Split { children, .. } => children.iter().enumerate().find_map(|(i, child)| {
            let mut path = slot(&child.node, view)?;
            path.insert(0, i);
            Some(path)
        }),
    }
}

fn node_mut<'a>(node: &'a mut Node, path: &[usize]) -> Option<&'a mut Node> {
    let Some((&first, rest)) = path.split_first() else {
        return Some(node);
    };
    match node {
        Node::Split { children, .. } => node_mut(&mut children.get_mut(first)?.node, rest),
        _ => None,
    }
}

fn reorder(root: &mut Node, view: ViewId, place: Place) -> bool {
    let Place::Beside {
        target,
        axis: along,
        after,
    } = place
    else {
        return false;
    };
    let Some(target) = slot(root, target) else {
        return false;
    };
    let Some((&target_index, parent)) = target.split_last() else {
        return false;
    };
    let Some(Node::Split { axis, children }) = node_mut(root, parent) else {
        return false;
    };
    if *axis != along {
        return false;
    }
    let Some(index) = children
        .iter()
        .position(|child| matches!(child.node, Node::View { id } if id == view))
    else {
        return false;
    };
    let child = children.remove(index);
    let target_index = target_index - usize::from(index < target_index);
    children.insert(target_index + usize::from(after), child);
    true
}

fn remove(root: &mut Node, view: ViewId) -> Option<Size> {
    let path = slot(root, view)?;
    let node = node_mut(root, &path)?;
    if let Node::Tabs { selected, views } = node {
        let index = views.iter().position(|&v| v == view)?;
        views.remove(index);
        if *selected > index || *selected == views.len() {
            *selected -= 1;
        }
        if let [only] = views[..] {
            *node = Node::View { id: only };
        }
        return None;
    }
    let (&index, parent) = path.split_last()?;
    let parent = node_mut(root, parent)?;
    let Node::Split { children, .. } = parent else {
        return None;
    };
    let removed = children.remove(index);
    if children.len() == 1 {
        *parent = children.remove(0).node;
    }
    Some(removed.size)
}

fn insert(root: &mut Node, view: ViewId, place: Place, size: Size) -> Result<(), Problem> {
    let target = place.target();
    let path = slot(root, target).ok_or(Problem::UnknownView(target))?;
    match place {
        Place::TabWith(_) => {
            let node = node_mut(root, &path).expect("slot path");
            match node {
                Node::Tabs { selected, views } => {
                    views.push(view);
                    *selected = views.len() - 1;
                }
                _ => {
                    *node = Node::Tabs {
                        selected: 1,
                        views: vec![target, view],
                    }
                }
            }
        }
        Place::Beside {
            axis: along, after, ..
        } => {
            let new = Child {
                node: Node::View { id: view },
                size,
            };
            if let Some((&index, parent)) = path.split_last()
                && let Some(Node::Split { axis, children }) = node_mut(root, parent)
                && *axis == along
            {
                children.insert(index + usize::from(after), new);
                return Ok(());
            }
            let node = node_mut(root, &path).expect("slot path");
            let existing = Child {
                node: std::mem::replace(node, Node::View { id: view }),
                size: Size::FILL,
            };
            *node = Node::Split {
                axis: along,
                children: if after {
                    vec![existing, new]
                } else {
                    vec![new, existing]
                },
            };
        }
    }
    Ok(())
}

/// What a project saved for its composition, kept as is even when this version cannot
/// read it. Only deserialization builds an unreadable one.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SavedComposition(Saved);

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
enum Saved {
    Supported(Composition),
    Unsupported(Value),
}

impl SavedComposition {
    /// `None` when this version cannot read it, so it cannot be borrowed.
    pub fn supported(&self) -> Option<&Composition> {
        match &self.0 {
            Saved::Supported(composition) => Some(composition),
            Saved::Unsupported(_) => None,
        }
    }

    /// The composition to draw: the saved one, or the default when it cannot be read.
    pub fn shown(&self) -> Composition {
        self.supported().cloned().unwrap_or_default()
    }
}

impl From<Composition> for SavedComposition {
    fn from(composition: Composition) -> Self {
        Self(Saved::Supported(composition))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::SidebarWidth;
    use serde_json::json;

    fn px(px: u16) -> Size {
        Size::Px(NonZeroU16::new(px).unwrap())
    }

    fn root(composition: &Composition) -> Value {
        serde_json::to_value(composition.root()).unwrap()
    }

    fn view(id: u32, size: Value) -> Value {
        json!({"node": {"kind": "view", "id": id}, "size": size})
    }

    #[test]
    fn the_default_is_the_legacy_sidebar_layout_and_round_trips() {
        let right = Composition::from_sidebar(SidebarLayout {
            side: SidebarSide::Right,
            width: SidebarWidth::new(360).unwrap(),
        });
        let saved = serde_json::to_value(&right).unwrap();
        assert_eq!(
            saved,
            json!({
                "version": 1,
                "views": [
                    {"id": 0, "component": "conversation"},
                    {"id": 1, "component": "session_list"},
                    {"id": 2, "component": "composer"},
                ],
                "root": {"kind": "split", "axis": "row", "children": [
                    {"node": {"kind": "split", "axis": "column", "children": [
                        view(0, json!({"fill": 1})),
                        view(2, json!("auto")),
                    ]}, "size": {"fill": 1}},
                    view(1, json!({"px": 360})),
                ]},
            })
        );
        assert_eq!(serde_json::from_value::<Composition>(saved).unwrap(), right);

        let default = Composition::default();
        assert_eq!(default, Composition::from_sidebar(SidebarLayout::default()));
        assert_eq!(root(&default)["children"][0], view(1, json!({"px": 264})));
        assert_eq!(
            default.views().collect::<Vec<_>>(),
            [
                (ViewId::MAIN, Component::Conversation),
                (SESSION_LIST, Component::SessionList),
                (COMPOSER, Component::Composer),
            ]
        );
    }

    #[test]
    fn moving_and_tabbing_an_occurrence_places_it_once_and_keeps_sizes() {
        let second = ViewId(7);
        let opened = Composition::default()
            .edit(Edit::Open {
                view: second,
                component: Component::Conversation,
                at: Place::Beside {
                    target: ViewId::MAIN,
                    axis: Axis::Row,
                    after: true,
                },
                size: px(400),
            })
            .unwrap();
        assert_eq!(
            root(&opened)["children"][1]["node"]["children"][0],
            json!({"node": {"kind": "split", "axis": "row", "children": [
                view(0, json!({"fill": 1})),
                view(7, json!({"px": 400})),
            ]}, "size": {"fill": 1}})
        );

        let tabbed = opened
            .edit(Edit::Move {
                view: second,
                to: Place::TabWith(ViewId::MAIN),
            })
            .unwrap();
        let column = json!({"kind": "split", "axis": "column", "children": [
            {"node": {"kind": "tabs", "selected": 1, "views": [0, 7]}, "size": {"fill": 1}},
            view(2, json!("auto")),
        ]});
        assert_eq!(root(&tabbed)["children"][1]["node"], column);
        for to in [
            Place::TabWith(second),
            Place::Beside {
                target: second,
                axis: Axis::Column,
                after: false,
            },
        ] {
            assert_eq!(
                tabbed.edit(Edit::Move { view: second, to }),
                Ok(tabbed.clone())
            );
        }
        let selected = tabbed.edit(Edit::SelectTab(ViewId::MAIN)).unwrap();
        assert_eq!(
            root(&selected)["children"][1]["node"]["children"][0]["node"]["selected"],
            0
        );

        let moved = tabbed
            .edit(Edit::Move {
                view: SESSION_LIST,
                to: Place::Beside {
                    target: COMPOSER,
                    axis: Axis::Row,
                    after: true,
                },
            })
            .unwrap();
        assert_eq!(
            root(&moved),
            json!({"kind": "split", "axis": "column", "children": [
                {"node": {"kind": "tabs", "selected": 1, "views": [0, 7]}, "size": {"fill": 1}},
                {"node": {"kind": "split", "axis": "row", "children": [
                    view(2, json!({"fill": 1})),
                    view(1, json!({"px": 264})),
                ]}, "size": "auto"},
            ]})
        );

        let untabbed = moved
            .edit(Edit::Move {
                view: ViewId::MAIN,
                to: Place::Beside {
                    target: second,
                    axis: Axis::Column,
                    after: true,
                },
            })
            .unwrap();
        assert_eq!(
            root(&untabbed)["children"],
            json!([
                view(7, json!({"fill": 1})),
                view(0, json!({"fill": 1})),
                root(&moved)["children"][1],
            ])
        );
        assert_eq!(
            untabbed.views().collect::<Vec<_>>(),
            opened.views().collect::<Vec<_>>()
        );
    }

    #[test]
    fn sibling_reorders_preserve_sizes_and_same_position_preserves_the_document() {
        let default = Composition::default();
        for sizes in [
            vec![Size::FILL, Size::Auto],
            vec![px(360), px(120)],
            vec![
                Size::Fill(NonZeroU16::new(2).unwrap()),
                Size::Fill(NonZeroU16::new(3).unwrap()),
            ],
            vec![Size::Auto, px(120)],
        ] {
            let original = default
                .edit(Edit::Resize {
                    split: NodePath(vec![1]),
                    sizes,
                })
                .unwrap();
            for (view, target, after) in [
                (ViewId::MAIN, COMPOSER, true),
                (COMPOSER, ViewId::MAIN, false),
            ] {
                let same_position = Edit::Move {
                    view,
                    to: Place::Beside {
                        target,
                        axis: Axis::Column,
                        after: !after,
                    },
                };
                assert_eq!(original.edit(same_position.clone()).unwrap(), original);
                let reordered = original
                    .edit(Edit::Move {
                        view,
                        to: Place::Beside {
                            target,
                            axis: Axis::Column,
                            after,
                        },
                    })
                    .unwrap();
                let mut expected = serde_json::to_value(&original).unwrap();
                expected["root"]["children"][1]["node"]["children"]
                    .as_array_mut()
                    .unwrap()
                    .reverse();
                assert_eq!(serde_json::to_value(&reordered).unwrap(), expected);
                assert_eq!(reordered.edit(same_position).unwrap(), original);
            }
        }
    }

    #[test]
    fn refused_edits_change_nothing_and_required_views_stay() {
        let second = ViewId(7);
        let base = Composition::default()
            .edit(Edit::Open {
                view: second,
                component: Component::Conversation,
                at: Place::TabWith(ViewId::MAIN),
                size: Size::FILL,
            })
            .unwrap();
        let before = base.clone();
        let beside_main = Place::Beside {
            target: ViewId::MAIN,
            axis: Axis::Row,
            after: false,
        };
        let open = |view, component, at| Edit::Open {
            view,
            component,
            at,
            size: Size::FILL,
        };
        for (edit, problem) in [
            (Edit::Close(COMPOSER), Problem::Count(Component::Composer)),
            (
                open(ViewId(9), Component::Composer, beside_main),
                Problem::Count(Component::Composer),
            ),
            (
                open(ViewId(9), Component::SessionList, beside_main),
                Problem::Count(Component::SessionList),
            ),
            (
                open(second, Component::Conversation, beside_main),
                Problem::DuplicateView(second),
            ),
            (
                open(COMPOSER, Component::Conversation, beside_main),
                Problem::DuplicateView(COMPOSER),
            ),
            (
                open(ViewId(9), Component::Conversation, Place::TabWith(COMPOSER)),
                Problem::TabNotSurface(COMPOSER),
            ),
            (
                Edit::Move {
                    view: SESSION_LIST,
                    to: Place::TabWith(ViewId::MAIN),
                },
                Problem::TabNotSurface(SESSION_LIST),
            ),
            (
                Edit::Move {
                    view: ViewId(42),
                    to: beside_main,
                },
                Problem::UnknownView(ViewId(42)),
            ),
            (
                Edit::Resize {
                    split: NodePath(vec![0]),
                    sizes: vec![Size::FILL, Size::FILL],
                },
                Problem::NoSplit,
            ),
            (
                Edit::Resize {
                    split: NodePath::default(),
                    sizes: vec![Size::FILL],
                },
                Problem::SizeCount,
            ),
            (Edit::SelectTab(COMPOSER), Problem::NotTabbed(COMPOSER)),
        ] {
            assert_eq!(base.edit(edit.clone()), Err(problem), "{edit:?}");
        }
        assert_eq!(base, before);

        let single = base.edit(Edit::Close(second)).unwrap();
        assert_eq!(single, Composition::default());
        assert_eq!(
            single.edit(Edit::Close(ViewId::MAIN)),
            Err(Problem::Count(Component::Conversation))
        );
        let resized = single
            .edit(Edit::Resize {
                split: NodePath(vec![1]),
                sizes: vec![px(600), Size::Fill(NonZeroU16::new(2).unwrap())],
            })
            .unwrap();
        assert_eq!(
            root(&resized)["children"][1]["node"]["children"][1]["size"],
            json!({"fill": 2})
        );
        let listless = single.edit(Edit::Close(SESSION_LIST)).unwrap();
        assert_eq!(root(&listless), root(&single)["children"][1]["node"]);
    }

    #[test]
    fn files_opens_beside_or_tabbed_with_a_conversation_but_only_once() {
        let files = ViewId(7);
        let beside = Composition::default()
            .edit(Edit::Open {
                view: files,
                component: Component::Files,
                at: Place::Beside {
                    target: ViewId::MAIN,
                    axis: Axis::Row,
                    after: true,
                },
                size: px(300),
            })
            .unwrap();
        assert_eq!(
            root(&beside)["children"][1]["node"]["children"][0]["node"]["children"],
            json!([view(0, json!({"fill": 1})), view(7, json!({"px": 300}))])
        );
        assert_eq!(
            serde_json::to_value(&beside).unwrap()["views"][3],
            json!({"id": 7, "component": "files"})
        );
        assert_eq!(
            serde_json::from_value::<Composition>(serde_json::to_value(&beside).unwrap()).unwrap(),
            beside
        );

        let tabbed = beside
            .edit(Edit::Move {
                view: files,
                to: Place::TabWith(ViewId::MAIN),
            })
            .unwrap();
        assert_eq!(
            root(&tabbed)["children"][1]["node"]["children"][0]["node"],
            json!({"kind": "tabs", "selected": 1, "views": [0, 7]})
        );
        assert_eq!(
            tabbed.edit(Edit::Close(files)).unwrap(),
            Composition::default()
        );

        let second = Edit::Open {
            view: ViewId(8),
            component: Component::Files,
            at: Place::TabWith(ViewId::MAIN),
            size: Size::FILL,
        };
        assert_eq!(tabbed.edit(second), Err(Problem::Count(Component::Files)));
        assert_eq!(
            tabbed.edit(Edit::Move {
                view: COMPOSER,
                to: Place::TabWith(files),
            }),
            Err(Problem::TabNotSurface(COMPOSER))
        );
        assert!(
            Composition::default()
                .views()
                .all(|(_, c)| c != Component::Files)
        );
    }

    #[test]
    fn changes_tabs_with_files_and_a_conversation_but_opens_only_once() {
        let (files, changes) = (ViewId(7), ViewId(8));
        let open = |view, component| Edit::Open {
            view,
            component,
            at: Place::TabWith(ViewId::MAIN),
            size: Size::FILL,
        };
        let both = Composition::default()
            .edit(open(files, Component::Files))
            .unwrap()
            .edit(open(changes, Component::Changes))
            .unwrap();
        assert_eq!(
            root(&both)["children"][1]["node"]["children"][0]["node"],
            json!({"kind": "tabs", "selected": 2, "views": [0, 7, 8]})
        );
        assert_eq!(
            serde_json::to_value(&both).unwrap()["views"][4],
            json!({"id": 8, "component": "changes"})
        );
        assert_eq!(
            serde_json::from_value::<Composition>(serde_json::to_value(&both).unwrap()).unwrap(),
            both
        );
        assert_eq!(
            both.edit(open(ViewId(9), Component::Changes)),
            Err(Problem::Count(Component::Changes))
        );
    }

    fn deepest_main(depth: usize) -> Result<Composition, Problem> {
        let mut composition = Composition::default();
        for level in 3..depth {
            composition = composition.edit(Edit::Open {
                view: ViewId(10 + level as u32),
                component: Component::Conversation,
                at: Place::Beside {
                    target: ViewId::MAIN,
                    axis: if level % 2 == 0 {
                        Axis::Column
                    } else {
                        Axis::Row
                    },
                    after: true,
                },
                size: Size::FILL,
            })?;
        }
        Ok(composition)
    }

    #[test]
    fn documents_this_version_cannot_read_are_kept_whole() {
        let valid = serde_json::to_value(Composition::default()).unwrap();
        let with = |changes: &[(&str, Value)]| {
            let mut document = valid.clone();
            for (pointer, value) in changes {
                match document.pointer_mut(pointer) {
                    Some(slot) => *slot = value.clone(),
                    None => document["views"]
                        .as_array_mut()
                        .unwrap()
                        .push(value.clone()),
                }
            }
            document
        };
        let extra = |pointer: &str| {
            let mut document = valid.clone();
            let object = document.pointer_mut(pointer).unwrap();
            object
                .as_object_mut()
                .unwrap()
                .insert("pinned".into(), json!(true));
            document
        };
        let deep = serde_json::to_value(deepest_main(MAX_DEPTH).unwrap()).unwrap();
        let mut too_deep = deep.clone();
        *too_deep.pointer_mut("/root/children/1/node").unwrap() = json!({"kind": "split", "axis": "row", "children": [
            {"node": too_deep["root"]["children"][1]["node"], "size": {"fill": 1}},
            view(99, json!({"fill": 1})),
        ]});
        too_deep["views"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": 99, "component": "conversation"}));
        let wide = |count: u32| {
            json!({
                "version": 1,
                "views": (0..count)
                    .map(|id| json!({"id": id, "component": if id == 0 { "composer" } else { "conversation" }}))
                    .collect::<Vec<_>>(),
                "root": {"kind": "split", "axis": "row", "children": (0..count)
                    .map(|id| view(id, json!({"fill": 1})))
                    .collect::<Vec<_>>()},
            })
        };
        assert!(serde_json::from_value::<Composition>(deep).is_ok());
        assert!(serde_json::from_value::<Composition>(wide(255)).is_ok());
        let conversation = json!({"id": 3, "component": "conversation"});

        for (document, why) in [
            (with(&[("/version", json!(2))]), "version 2"),
            (extra(""), "unknown field `pinned`"),
            (extra("/root"), "unknown field `pinned`"),
            (extra("/root/children/0"), "unknown field `pinned`"),
            (extra("/root/children/0/size"), ""),
            (extra("/views/0"), "unknown field `pinned`"),
            (
                with(&[("/views/0/component", json!("activity"))]),
                "unknown variant",
            ),
            (with(&[("/root/kind", json!("grid"))]), "unknown variant"),
            (
                with(&[("/views/2/id", json!(0))]),
                "view 0 is declared twice",
            ),
            (
                with(&[("/root/children/0/node/id", json!(9))]),
                "view 9 is not declared",
            ),
            (
                with(&[("/root/children/0/node/id", json!(0))]),
                "view 0 is placed twice",
            ),
            (
                with(&[("/views/3", conversation.clone())]),
                "view 3 is not placed",
            ),
            (
                with(&[("/views/2/component", json!("conversation"))]),
                "number of Composer",
            ),
            (
                with(&[("/views/1/component", json!("composer"))]),
                "number of Composer",
            ),
            (
                json!({
                    "version": 1,
                    "views": [
                        {"id": 1, "component": "session_list"},
                        {"id": 2, "component": "composer"},
                    ],
                    "root": {"kind": "split", "axis": "row", "children": [
                        view(1, json!({"px": 302})),
                        view(2, json!({"fill": 1})),
                    ]},
                }),
                "number of Conversation",
            ),
            (with(&[("/root/children/0/size", json!({"px": 0}))]), ""),
            (with(&[("/root/children/0/size", json!({"fill": 0}))]), ""),
            (with(&[("/root/children/0/size", json!({"px": 1.5}))]), ""),
            (
                with(&[("/root/children/0/size", json!({"px": 70_000}))]),
                "",
            ),
            (
                with(&[("/root/children/0/size", json!({"px": 9, "fill": 1}))]),
                "",
            ),
            (
                with(&[(
                    "/root/children/1/node/children",
                    json!([view(0, json!({"fill": 1}))]),
                )]),
                "fewer than two",
            ),
            (
                with(&[(
                    "/root/children/1/node",
                    json!({"kind": "tabs", "selected": 0, "views": [0, 2]}),
                )]),
                "view 2 is tabbed",
            ),
            (
                with(&[
                    (
                        "/root/children/1/node/children/0/node",
                        json!({"kind": "tabs", "selected": 2, "views": [0, 3]}),
                    ),
                    ("/views/3", conversation),
                ]),
                "selected tab",
            ),
            (too_deep, "deeper than 32"),
            (wide(256), "more than 256"),
            (json!("wide"), ""),
            (Value::Null, ""),
        ] {
            let error = serde_json::from_value::<Composition>(document.clone()).unwrap_err();
            assert!(error.to_string().contains(why), "{error} for {document}");
            let saved: SavedComposition = serde_json::from_value(document.clone()).unwrap();
            assert_eq!(saved.supported(), None, "{document}");
            assert_eq!(saved.shown(), Composition::default());
            assert_ne!(saved, Composition::default().into());
            assert_eq!(serde_json::to_value(&saved).unwrap(), document);
        }

        let saved: SavedComposition = serde_json::from_value(valid.clone()).unwrap();
        assert_eq!(saved.supported(), Some(&Composition::default()));
        assert_eq!(serde_json::to_value(&saved).unwrap(), valid);
    }

    #[test]
    fn edits_cannot_nest_past_the_depth_limit() {
        let deepest = deepest_main(MAX_DEPTH).unwrap();
        assert_eq!(deepest.views().count(), MAX_DEPTH);
        assert_eq!(deepest_main(MAX_DEPTH + 1), Err(Problem::TooDeep));
    }

    fn open(c: &Composition, view: u32, at: Place) -> Composition {
        c.edit(Edit::Open {
            view: ViewId(view),
            component: Component::Conversation,
            at,
            size: Size::FILL,
        })
        .unwrap()
    }

    #[test]
    fn reordering_a_tab_keeps_the_shown_one_and_refuses_an_untabbed_view() {
        let c = open(&Composition::default(), 3, Place::TabWith(ViewId::MAIN));
        let c = open(&c, 4, Place::TabWith(ViewId::MAIN));
        let c = c.edit(Edit::SelectTab(ViewId(3))).unwrap();
        let tabs = |c: &Composition| {
            let (views, selected) = tabs_of(c.root()).unwrap();
            (views.to_vec(), views[selected])
        };
        assert_eq!(
            tabs(&c),
            (vec![ViewId::MAIN, ViewId(3), ViewId(4)], ViewId(3))
        );
        let moved = c
            .edit(Edit::Reorder {
                view: ViewId::MAIN,
                index: 9,
            })
            .unwrap();
        assert_eq!(
            tabs(&moved),
            (vec![ViewId(3), ViewId(4), ViewId::MAIN], ViewId(3))
        );
        let moved = moved
            .edit(Edit::Reorder {
                view: ViewId(4),
                index: 0,
            })
            .unwrap();
        assert_eq!(
            tabs(&moved),
            (vec![ViewId(4), ViewId(3), ViewId::MAIN], ViewId(3))
        );
        assert_eq!(
            c.edit(Edit::Reorder {
                view: COMPOSER,
                index: 0
            }),
            Err(Problem::NotTabbed(COMPOSER))
        );
    }

    #[test]
    fn closing_a_panes_last_tab_collapses_its_split() {
        let c = open(
            &Composition::default(),
            3,
            Place::Beside {
                target: ViewId::MAIN,
                axis: Axis::Row,
                after: true,
            },
        );
        let c = open(&c, 4, Place::TabWith(ViewId(3)));
        let one_pane = c
            .edit(Edit::Close(ViewId(3)))
            .unwrap()
            .edit(Edit::Close(ViewId(4)))
            .unwrap();
        assert_eq!(one_pane, Composition::default());
    }

    fn tabs_of(node: &Node) -> Option<(&[ViewId], usize)> {
        match node {
            Node::Tabs { views, selected } => Some((views, *selected)),
            Node::View { .. } => None,
            Node::Split { children, .. } => children.iter().find_map(|c| tabs_of(&c.node)),
        }
    }
}
