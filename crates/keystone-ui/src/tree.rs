// The split into a tree state that owns the rows and a styled `Tree` element, the
// Left/Right/Enter key model, and the `Tree` key context follow gpui-base 0.7.3
// `tree.rs` and gpui-component 0.7.3 `tree.rs` (Copyright 2024 - 2026 Longbridge,
// Apache-2.0). Modified: rewritten around a flat arena and visible-row vector that
// splices on expand instead of rebuilding, with lazy children and `VirtualList` rows.

use crate::color::Color;
use crate::icon::{Icon, IconName};
use crate::label::Label;
use crate::theme::ActiveTheme;
use crate::tokens::{TextSize, radius};
use crate::virtual_list::{VirtualList, VirtualListState};
use gpui::{
    AnyElement, App, Context, Entity, EntityId, FocusHandle, Focusable, IntoElement, KeyBinding,
    ParentElement, RenderOnce, SharedString, StyleRefinement, Styled, Window, div, prelude::*, px,
};
use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;

gpui::actions!(tree, [CollapseOrParent, ExpandOrChild]);

const CONTEXT: &str = "Tree";
const INDENT: f32 = 14.;

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("left", CollapseOrParent, Some(CONTEXT)),
        KeyBinding::new("right", ExpandOrChild, Some(CONTEXT)),
    ]);
}

/// An index into a [`TreeModel`]'s node arena. Ids stay valid for the model's life.
pub type NodeId = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    File,
    Folder,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Children {
    Unloaded,
    Loading,
    Loaded(Range<NodeId>),
}

#[derive(Debug)]
enum Body {
    File,
    Folder { expanded: bool, children: Children },
}

#[derive(Debug)]
pub struct Node {
    parent: Option<NodeId>,
    depth: u16,
    name: SharedString,
    body: Body,
}

impl Node {
    pub fn parent(&self) -> Option<NodeId> {
        self.parent
    }

    pub fn name(&self) -> &SharedString {
        &self.name
    }

    pub fn kind(&self) -> NodeKind {
        match self.body {
            Body::File => NodeKind::File,
            Body::Folder { .. } => NodeKind::Folder,
        }
    }

    pub fn is_expanded(&self) -> bool {
        matches!(self.body, Body::Folder { expanded: true, .. })
    }

    /// The node's children, once its folder has loaded them.
    pub fn children(&self) -> Option<Range<NodeId>> {
        match &self.body {
            Body::Folder {
                children: Children::Loaded(range),
                ..
            } => Some(range.clone()),
            _ => None,
        }
    }
}

/// One drawn row: a node, or the placeholder under a folder still loading its children.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Node(NodeId),
    Loading(NodeId),
}

/// How one change rewrote the visible rows: `removed` rows at `at` became `inserted` rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Splice {
    pub at: usize,
    pub removed: usize,
    pub inserted: usize,
}

impl Splice {
    /// Where a row that sat at `row` before the change sits after it. A row that the
    /// change removed lands on the first inserted row, or else the row above the change.
    fn remap(self, row: usize) -> usize {
        if row < self.at {
            row
        } else if row < self.at + self.removed {
            if self.inserted > 0 {
                self.at
            } else {
                self.at - 1
            }
        } else {
            row - self.removed + self.inserted
        }
    }
}

/// A tree kept as a flat node arena plus the flat list of rows on screen. A folder's
/// children are contiguous in the arena, so a folder stores only their range. Expanding
/// splices the folder's visible descendants into `visible` at once and collapsing drains
/// them, so neither walks more than the rows it adds or removes.
#[derive(Debug, Default)]
pub struct TreeModel {
    nodes: Vec<Node>,
    visible: Vec<Row>,
}

impl TreeModel {
    pub fn new(roots: impl IntoIterator<Item = (impl Into<SharedString>, NodeKind)>) -> Self {
        let mut model = Self::default();
        let roots = model.push_nodes(None, 0, roots);
        model.visible = roots.map(Row::Node).collect();
        model
    }

    /// The rows on screen, top to bottom.
    pub fn visible(&self) -> &[Row] {
        &self.visible
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn depth(&self, row: Row) -> u16 {
        match row {
            Row::Node(id) => self.node(id).depth,
            Row::Loading(folder) => self.node(folder).depth + 1,
        }
    }

    /// The visible row showing `id`, found by scanning the visible rows.
    pub fn row_of(&self, id: NodeId) -> Option<usize> {
        self.visible.iter().position(|row| *row == Row::Node(id))
    }

    /// Fills `folder` with children and returns their ids, replacing any it had.
    /// Replaced children stay in the arena but are no longer reachable.
    pub fn set_children(
        &mut self,
        folder: NodeId,
        children: impl IntoIterator<Item = (impl Into<SharedString>, NodeKind)>,
    ) -> Range<NodeId> {
        self.replace_children(folder, children).0
    }

    /// Shows the children of the folder at `row`. A folder that never loaded its
    /// children shows a [`Row::Loading`] row until [`Self::set_children`] fills it.
    pub fn expand(&mut self, row: usize) -> Option<Splice> {
        let Row::Node(id) = self.visible[row] else {
            return None;
        };
        let Body::Folder { expanded, children } = &mut self.nodes[id as usize].body else {
            return None;
        };
        if *expanded {
            return None;
        }
        *expanded = true;
        if *children == Children::Unloaded {
            *children = Children::Loading;
        }
        let rows = self.rows_under(id);
        let inserted = rows.len();
        self.visible.splice(row + 1..row + 1, rows);
        Some(Splice {
            at: row + 1,
            removed: 0,
            inserted,
        })
    }

    /// Hides the descendants of the folder at `row`. Nested folders keep their own
    /// expanded state, so expanding again restores them.
    pub fn collapse(&mut self, row: usize) -> Option<Splice> {
        let Row::Node(id) = self.visible[row] else {
            return None;
        };
        let Body::Folder { expanded, .. } = &mut self.nodes[id as usize].body else {
            return None;
        };
        if !*expanded {
            return None;
        }
        *expanded = false;
        let end = self.descendants_end(row);
        self.visible.drain(row + 1..end);
        Some(Splice {
            at: row + 1,
            removed: end - row - 1,
            inserted: 0,
        })
    }

    /// The row of the nearest folder above `row` that contains it.
    pub fn parent_row(&self, row: usize) -> Option<usize> {
        let depth = self.depth(self.visible[row]);
        self.visible[..row]
            .iter()
            .rposition(|above| self.depth(*above) < depth)
    }

    fn push_nodes(
        &mut self,
        parent: Option<NodeId>,
        depth: u16,
        nodes: impl IntoIterator<Item = (impl Into<SharedString>, NodeKind)>,
    ) -> Range<NodeId> {
        let start = self.nodes.len();
        self.nodes
            .extend(nodes.into_iter().map(|(name, kind)| Node {
                parent,
                depth,
                name: name.into(),
                body: match kind {
                    NodeKind::File => Body::File,
                    NodeKind::Folder => Body::Folder {
                        expanded: false,
                        children: Children::Unloaded,
                    },
                },
            }));
        let end = NodeId::try_from(self.nodes.len()).expect("fewer than 2^32 nodes");
        start as NodeId..end
    }

    fn replace_children(
        &mut self,
        folder: NodeId,
        children: impl IntoIterator<Item = (impl Into<SharedString>, NodeKind)>,
    ) -> (Range<NodeId>, Option<Splice>) {
        let depth = self.node(folder).depth + 1;
        let range = self.push_nodes(Some(folder), depth, children);
        let Body::Folder {
            expanded,
            children: slot,
        } = &mut self.nodes[folder as usize].body
        else {
            panic!("node {folder} is a file and cannot have children");
        };
        *slot = Children::Loaded(range.clone());
        if !*expanded {
            return (range, None);
        }
        let Some(row) = self.row_of(folder) else {
            return (range, None);
        };
        let end = self.descendants_end(row);
        let rows = self.rows_under(folder);
        let inserted = rows.len();
        self.visible.splice(row + 1..end, rows);
        let splice = Splice {
            at: row + 1,
            removed: end - row - 1,
            inserted,
        };
        (range, Some(splice))
    }

    /// The end of the visible rows below `row` that are its descendants.
    fn descendants_end(&self, row: usize) -> usize {
        let depth = self.depth(self.visible[row]);
        self.visible[row + 1..]
            .iter()
            .position(|below| self.depth(*below) <= depth)
            .map_or(self.visible.len(), |offset| row + 1 + offset)
    }

    /// The rows an expanded `folder` shows below itself, in order. Walks only
    /// expanded folders, so it costs as much as the rows it returns.
    fn rows_under(&self, folder: NodeId) -> Vec<Row> {
        let mut rows = Vec::new();
        let mut stack = Vec::new();
        let descend = |id: NodeId, rows: &mut Vec<Row>, stack: &mut Vec<Range<NodeId>>| match &self
            .node(id)
            .body
        {
            Body::Folder {
                expanded: true,
                children: Children::Loading,
            } => rows.push(Row::Loading(id)),
            Body::Folder {
                expanded: true,
                children: Children::Loaded(range),
            } => stack.push(range.clone()),
            _ => {}
        };
        descend(folder, &mut rows, &mut stack);
        while let Some(next) = stack.last_mut() {
            match next.next() {
                Some(id) => {
                    rows.push(Row::Node(id));
                    descend(id, &mut rows, &mut stack);
                }
                None => {
                    stack.pop();
                }
            }
        }
        rows
    }
}

/// What a row hands to the caller's `render_row`.
#[derive(Clone, Debug)]
pub struct TreeEntry {
    pub id: NodeId,
    pub name: SharedString,
    pub kind: NodeKind,
    pub depth: u16,
    pub expanded: bool,
}

/// A key or click asked the caller to do something with a node.
enum Request {
    Open(NodeId),
    Load(NodeId),
}

/// The model behind a [`Tree`] and the [`VirtualListState`] that draws its visible
/// rows. Every change goes through here, so the list's row count and highlight
/// always match the model.
pub struct TreeState {
    model: TreeModel,
    list: Entity<VirtualListState>,
    /// The view that last drew the tree. A cached view redraws only for its own
    /// notifications, so every change notifies it too.
    host: Cell<Option<EntityId>>,
}

impl TreeState {
    pub fn new(model: TreeModel, cx: &mut App) -> Self {
        let rows = model.visible.len();
        Self {
            model,
            list: cx.new(|cx| VirtualListState::new(rows, cx)),
            host: Cell::new(None),
        }
    }

    pub fn model(&self) -> &TreeModel {
        &self.model
    }

    pub fn highlighted_row(&self, cx: &App) -> Option<usize> {
        self.list.read(cx).highlighted()
    }

    pub fn highlighted_node(&self, cx: &App) -> Option<NodeId> {
        match self.model.visible[self.highlighted_row(cx)?] {
            Row::Node(id) => Some(id),
            Row::Loading(_) => None,
        }
    }

    /// Expands the folder at `row`. Returns the folder when its children have never
    /// been requested, so the caller can load them and call [`Self::set_children`].
    pub fn expand(&mut self, row: usize, cx: &mut Context<Self>) -> Option<NodeId> {
        let request = match self.model.visible[row] {
            Row::Node(id)
                if matches!(
                    self.model.node(id).body,
                    Body::Folder {
                        children: Children::Unloaded,
                        ..
                    }
                ) =>
            {
                Some(id)
            }
            _ => None,
        };
        let splice = self.model.expand(row);
        self.apply(splice, cx);
        request.filter(|_| splice.is_some())
    }

    pub fn collapse(&mut self, row: usize, cx: &mut Context<Self>) {
        let splice = self.model.collapse(row);
        self.apply(splice, cx);
    }

    pub fn toggle(&mut self, row: usize, cx: &mut Context<Self>) -> Option<NodeId> {
        match self.model.visible[row] {
            Row::Node(id) if self.model.node(id).is_expanded() => {
                self.collapse(row, cx);
                None
            }
            _ => self.expand(row, cx),
        }
    }

    pub fn set_children(
        &mut self,
        folder: NodeId,
        children: impl IntoIterator<Item = (impl Into<SharedString>, NodeKind)>,
        cx: &mut Context<Self>,
    ) -> Range<NodeId> {
        let (range, splice) = self.model.replace_children(folder, children);
        self.apply(splice, cx);
        range
    }

    /// The list that draws the visible rows, for its scroll handle or focus.
    pub fn list(&self) -> &Entity<VirtualListState> {
        &self.list
    }

    /// Highlights the visible row at `row` and scrolls it into view.
    pub fn highlight(&mut self, row: usize, cx: &mut Context<Self>) {
        self.list.update(cx, |list, cx| {
            list.move_to(row);
            cx.notify();
        });
        self.redraw(cx);
    }

    fn redraw(&self, cx: &mut Context<Self>) {
        cx.notify();
        if let Some(host) = self.host.get() {
            App::notify(cx, host);
        }
    }

    fn apply(&mut self, splice: Option<Splice>, cx: &mut Context<Self>) {
        let Some(splice) = splice else {
            return;
        };
        let rows = self.model.visible.len();
        self.list.update(cx, |list, cx| {
            let highlighted = list.highlighted();
            list.set_count(rows);
            if let Some(row) = highlighted {
                list.move_to(splice.remap(row));
            }
            cx.notify();
        });
        self.redraw(cx);
    }

    /// Left collapses an expanded folder, and otherwise moves to the parent.
    fn collapse_or_parent(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.highlighted_row(cx) else {
            return;
        };
        match self.model.visible[row] {
            Row::Node(id) if self.model.node(id).is_expanded() => self.collapse(row, cx),
            _ => {
                if let Some(parent) = self.model.parent_row(row) {
                    self.highlight(parent, cx);
                }
            }
        }
    }

    /// Right expands a collapsed folder, and moves into an expanded one.
    fn expand_or_child(&mut self, cx: &mut Context<Self>) -> Option<Request> {
        let row = self.highlighted_row(cx)?;
        let Row::Node(id) = self.model.visible[row] else {
            return None;
        };
        let node = self.model.node(id);
        match node.body {
            Body::File => None,
            Body::Folder {
                expanded: false, ..
            } => self.expand(row, cx).map(Request::Load),
            Body::Folder { expanded: true, .. } => {
                let depth = node.depth;
                let child = self.model.visible.get(row + 1)?;
                if self.model.depth(*child) > depth {
                    self.highlight(row + 1, cx);
                }
                None
            }
        }
    }

    /// Enter and a row click toggle a folder and open a file.
    fn confirm(&mut self, row: usize, cx: &mut Context<Self>) -> Option<Request> {
        let Row::Node(id) = self.model.visible[row] else {
            return None;
        };
        match self.model.node(id).body {
            Body::File => Some(Request::Open(id)),
            Body::Folder { .. } => self.toggle(row, cx).map(Request::Load),
        }
    }

    fn entry(&self, row: Row) -> Option<TreeEntry> {
        let Row::Node(id) = row else {
            return None;
        };
        let node = self.model.node(id);
        Some(TreeEntry {
            id,
            name: node.name.clone(),
            kind: node.kind(),
            depth: node.depth,
            expanded: node.is_expanded(),
        })
    }
}

impl Focusable for TreeState {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.list.read(cx).focus_handle(cx)
    }
}

type RenderRow = Rc<dyn Fn(&TreeEntry, bool, &mut Window, &mut App) -> AnyElement + 'static>;
type NodeHandler = Rc<dyn Fn(&NodeId, &mut Window, &mut App) + 'static>;

/// A keyboard-driven tree over [`VirtualList`] that draws only the rows on screen.
/// Each row is one flat element indented by its depth, with a chevron for folders
/// and the caller's `render_row` content after it. Rows are named `{id}-row-{index}`
/// and chevrons `{id}-chevron-{index}`.
#[derive(IntoElement)]
pub struct Tree {
    id: SharedString,
    state: Entity<TreeState>,
    render_row: RenderRow,
    on_open: Option<NodeHandler>,
    on_expand: Option<NodeHandler>,
    style: StyleRefinement,
}

impl Tree {
    pub fn new(
        id: impl Into<SharedString>,
        state: &Entity<TreeState>,
        render_row: impl Fn(&TreeEntry, bool, &mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            state: state.clone(),
            render_row: Rc::new(render_row),
            on_open: None,
            on_expand: None,
            style: StyleRefinement::default(),
        }
    }

    /// Called with the file chosen by Enter or a click.
    pub fn on_open(mut self, handler: impl Fn(&NodeId, &mut Window, &mut App) + 'static) -> Self {
        self.on_open = Some(Rc::new(handler));
        self
    }

    /// Called when a folder whose children were never loaded expands. The tree shows
    /// a Loading row under it until the caller calls [`TreeState::set_children`].
    pub fn on_expand(mut self, handler: impl Fn(&NodeId, &mut Window, &mut App) + 'static) -> Self {
        self.on_expand = Some(Rc::new(handler));
        self
    }
}

impl Styled for Tree {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

#[derive(Clone)]
struct Handlers {
    state: Entity<TreeState>,
    host: EntityId,
    on_open: Option<NodeHandler>,
    on_expand: Option<NodeHandler>,
}

impl Handlers {
    /// Runs `change` on the state, redraws the view this tree was drawn in, and passes
    /// on any request the change makes.
    fn run(
        &self,
        window: &mut Window,
        cx: &mut App,
        change: impl FnOnce(&mut TreeState, &mut Context<TreeState>) -> Option<Request>,
    ) {
        let request = self.state.update(cx, change);
        cx.notify(self.host);
        let (handler, id) = match request {
            Some(Request::Open(id)) => (&self.on_open, id),
            Some(Request::Load(id)) => (&self.on_expand, id),
            None => return,
        };
        if let Some(handler) = handler {
            handler(&id, window, cx);
        }
    }
}

impl RenderOnce for Tree {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let host = window.current_view();
        self.state.read(cx).host.set(Some(host));
        let handlers = Handlers {
            state: self.state.clone(),
            host,
            on_open: self.on_open,
            on_expand: self.on_expand,
        };
        let list = self.state.read(cx).list.clone();
        let rows = {
            let (id, handlers, render_row) = (self.id.clone(), handlers.clone(), self.render_row);
            move |ix: usize, highlighted: bool, window: &mut Window, cx: &mut App| {
                let state = handlers.state.read(cx);
                let row = state.model.visible[ix];
                let depth = state.model.depth(row);
                let entry = state.entry(row);
                let theme = cx.theme();
                let (height, fill) = (theme.density.control(px(24.)), theme.colors.selection_fill);
                let chevron = div()
                    .id(("chevron", ix))
                    .flex_none()
                    .size(px(16.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .when_some(
                        entry.as_ref().filter(|e| e.kind == NodeKind::Folder),
                        |d, entry| {
                            let icon = if entry.expanded {
                                IconName::Chevron
                            } else {
                                IconName::Right
                            };
                            let handlers = handlers.clone();
                            d.cursor_pointer()
                                .child(Icon::new(icon).size(px(12.)).color(Color::Muted))
                                .on_click(move |_, window, cx| {
                                    cx.stop_propagation();
                                    handlers.run(window, cx, |state, cx| {
                                        state.highlight(ix, cx);
                                        state.toggle(ix, cx).map(Request::Load)
                                    });
                                })
                        },
                    );
                let chevron = crate::automation::tag(chevron, format!("{id}-chevron-{ix}"));
                let content = match &entry {
                    Some(entry) => render_row(entry, highlighted, window, cx),
                    None => Label::new("Loading…")
                        .size(TextSize::Base)
                        .color(Color::Muted)
                        .into_any_element(),
                };
                div()
                    .h(height)
                    .flex()
                    .items_center()
                    .gap_1()
                    .pl(px(4. + INDENT * depth as f32))
                    .pr(px(8.))
                    .rounded(radius::MENU_ROW)
                    .cursor_pointer()
                    .when(highlighted, |d| d.bg(fill))
                    .child(chevron)
                    .child(content)
                    .into_any_element()
            }
        };
        let confirm = handlers.clone();
        let (left, right) = (handlers.clone(), handlers);
        let mut root = div()
            .flex()
            .flex_col()
            .key_context(CONTEXT)
            .on_action(move |_: &CollapseOrParent, window, cx| {
                left.run(window, cx, |state, cx| {
                    state.collapse_or_parent(cx);
                    None
                })
            })
            .on_action(move |_: &ExpandOrChild, window, cx| {
                right.run(window, cx, |state, cx| state.expand_or_child(cx))
            });
        root.style().refine(&self.style);
        root.child(
            VirtualList::new(self.id, &list, rows)
                .on_confirm(move |ix, window, cx| {
                    confirm.run(window, cx, |state, cx| state.confirm(*ix, cx))
                })
                .flex_1()
                .min_h_0(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(prefix: &str, n: usize) -> Vec<(String, NodeKind)> {
        (0..n)
            .map(|i| (format!("{prefix}{i}"), NodeKind::File))
            .collect()
    }

    fn names(model: &TreeModel) -> Vec<String> {
        model
            .visible()
            .iter()
            .map(|row| match *row {
                Row::Node(id) => model.node(id).name().to_string(),
                Row::Loading(_) => "…".into(),
            })
            .collect()
    }

    /// `a/` holding `a0 a1`, and `b/` holding `inner/` (holding `x`) and `b1`.
    fn sample() -> (TreeModel, NodeId, NodeId, NodeId) {
        let mut model = TreeModel::new([("a", NodeKind::Folder), ("b", NodeKind::Folder)]);
        model.set_children(0, files("a", 2));
        let b = model.set_children(1, [("inner", NodeKind::Folder), ("b1", NodeKind::File)]);
        let inner = b.start;
        model.set_children(inner, [("x", NodeKind::File)]);
        (model, 0, 1, inner)
    }

    #[test]
    fn expand_splices_visible_rows() {
        let (mut model, _, _, _) = sample();
        assert_eq!(names(&model), ["a", "b"]);
        let splice = model.expand(0).unwrap();
        assert_eq!(
            splice,
            Splice {
                at: 1,
                removed: 0,
                inserted: 2
            }
        );
        assert_eq!(names(&model), ["a", "a0", "a1", "b"]);
        assert_eq!(model.expand(0), None, "already expanded");
        assert_eq!(model.expand(1), None, "a file does not expand");

        let mut big = TreeModel::new([("big", NodeKind::Folder), ("after", NodeKind::File)]);
        big.set_children(0, files("f", 10_000));
        big.expand(0);
        assert_eq!(big.visible().len(), 10_002);
        assert_eq!(big.visible()[10_001], Row::Node(1));
        assert_eq!(big.depth(big.visible()[5_000]), 1);
    }

    #[test]
    fn collapse_removes_descendants() {
        let (mut model, a, b, inner) = sample();
        model.expand(0);
        let b_row = model.row_of(b).unwrap();
        model.expand(b_row);
        model.expand(model.row_of(inner).unwrap());
        assert_eq!(names(&model), ["a", "a0", "a1", "b", "inner", "x", "b1"]);
        let splice = model.collapse(model.row_of(a).unwrap()).unwrap();
        assert_eq!(splice.removed, 2);
        assert_eq!(names(&model), ["a", "b", "inner", "x", "b1"]);
        model.collapse(1);
        assert_eq!(names(&model), ["a", "b"]);
        assert_eq!(model.collapse(1), None, "already collapsed");
        assert_eq!(model.parent_row(0), None);
    }

    #[test]
    fn nested_expand_restores_inner_state() {
        let (mut model, _, b, inner) = sample();
        model.expand(model.row_of(b).unwrap());
        model.expand(model.row_of(inner).unwrap());
        model.collapse(model.row_of(b).unwrap());
        assert_eq!(names(&model), ["a", "b"]);
        assert!(model.node(inner).is_expanded());
        model.expand(model.row_of(b).unwrap());
        assert_eq!(names(&model), ["a", "b", "inner", "x", "b1"]);
        let x = model
            .row_of(model.node(inner).children().unwrap().start)
            .unwrap();
        assert_eq!(model.parent_row(x), model.row_of(inner));
        assert_eq!(model.parent_row(x - 1), model.row_of(b));
    }

    #[test]
    fn lazy_children_replace_loading_row() {
        let mut model = TreeModel::new([("lazy", NodeKind::Folder), ("after", NodeKind::File)]);
        model.expand(0);
        assert_eq!(
            model.visible(),
            [Row::Node(0), Row::Loading(0), Row::Node(1)]
        );
        assert_eq!(model.parent_row(1), Some(0));
        model.collapse(0);
        model.expand(0);
        assert_eq!(model.visible()[1], Row::Loading(0), "still loading");
        let (range, splice) = model.replace_children(0, files("c", 3));
        assert_eq!(
            splice,
            Some(Splice {
                at: 1,
                removed: 1,
                inserted: 3
            })
        );
        assert_eq!(names(&model), ["lazy", "c0", "c1", "c2", "after"]);
        assert_eq!(model.node(range.start).parent(), Some(0));

        model.collapse(0);
        let (_, splice) = model.replace_children(0, files("d", 1));
        assert_eq!(splice, None, "a collapsed folder changes no rows");
        model.expand(0);
        assert_eq!(names(&model), ["lazy", "d0", "after"]);
    }

    #[test]
    fn splice_remaps_rows() {
        let collapse = Splice {
            at: 4,
            removed: 3,
            inserted: 0,
        };
        assert_eq!(
            [2, 3, 4, 6, 7, 9].map(|row| collapse.remap(row)),
            [2, 3, 3, 3, 4, 6]
        );
        let load = Splice {
            at: 4,
            removed: 1,
            inserted: 5,
        };
        assert_eq!([3, 4, 5].map(|row| load.remap(row)), [3, 4, 9]);
    }
}
