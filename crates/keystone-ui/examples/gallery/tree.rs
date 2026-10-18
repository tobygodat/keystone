//! The tree page: a 50,000-node sample tree with a 10,000-child folder, nested
//! folders, and a folder that loads its children lazily.

use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, Window, div, prelude::*, px,
};
use keystone_ui::prelude::*;

const NODES: usize = 50_000;
const ARCHIVE_FOLDERS: usize = 40;

pub struct TreePage {
    state: Entity<TreeState>,
    opened: Option<NodeId>,
}

impl TreePage {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut model = TreeModel::new([
            ("src", NodeKind::Folder),
            ("10k-files", NodeKind::Folder),
            ("remote (lazy)", NodeKind::Folder),
            ("archive", NodeKind::Folder),
            ("README.md", NodeKind::File),
        ]);
        let src = model.set_children(
            0,
            [
                ("components", NodeKind::Folder),
                ("main.rs", NodeKind::File),
                ("lib.rs", NodeKind::File),
            ],
        );
        let components = model.set_children(
            src.start,
            [
                ("widgets", NodeKind::Folder),
                ("tree.rs", NodeKind::File),
                ("list.rs", NodeKind::File),
            ],
        );
        model.set_children(
            components.start,
            [("button.rs", NodeKind::File), ("label.rs", NodeKind::File)],
        );
        model.set_children(
            1,
            (0..10_000).map(|n| (format!("file-{n:05}.rs"), NodeKind::File)),
        );
        let batches = model.set_children(
            3,
            (0..ARCHIVE_FOLDERS).map(|n| (format!("batch-{n:02}"), NodeKind::Folder)),
        );
        let files = NODES - model.node_count();
        for (n, batch) in batches.enumerate() {
            let count = files / ARCHIVE_FOLDERS + usize::from(n < files % ARCHIVE_FOLDERS);
            model.set_children(
                batch,
                (0..count).map(|i| (format!("entry-{i:04}.log"), NodeKind::File)),
            );
        }
        Self {
            state: cx.new(|cx| TreeState::new(model, cx)),
            opened: None,
        }
    }

    fn load(&mut self, folder: NodeId, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1200))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.state.update(cx, |state, cx| {
                    state.set_children(
                        folder,
                        (1..=5).map(|n| (format!("fetched-{n}.txt"), NodeKind::File)),
                        cx,
                    );
                });
                cx.notify();
            });
        })
        .detach();
    }
}

impl Focusable for TreePage {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.state.read(cx).focus_handle(cx)
    }
}

impl Render for TreePage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);
        let model = state.model();
        let name =
            |id: Option<NodeId>| id.map_or("none".into(), |id| model.node(id).name().clone());
        let status = format!(
            "{} nodes · {} rows · highlighted {} · opened {}",
            model.node_count(),
            model.visible().len(),
            name(state.highlighted_node(cx)),
            name(self.opened),
        );
        v_flex()
            .flex_1()
            .min_h_0()
            .px_6()
            .pb_6()
            .gap_3()
            .child(
                div()
                    .automation_id("gallery-tree-status")
                    .child(Label::new(status).color(Color::Muted)),
            )
            .child(
                Tree::new("gallery-tree", &self.state, |entry, _, _, _| {
                    let icon = match entry.kind {
                        NodeKind::Folder => IconName::Folder,
                        NodeKind::File => IconName::File,
                    };
                    h_flex()
                        .gap_2()
                        .min_w_0()
                        .child(Icon::new(icon).size(px(14.)).color(Color::Muted))
                        .child(Label::new(entry.name.clone()).truncate())
                        .into_any_element()
                })
                .on_open(cx.listener(|this, id: &NodeId, _, cx| {
                    this.opened = Some(*id);
                    cx.notify();
                }))
                .on_expand(cx.listener(|this, id: &NodeId, _, cx| this.load(*id, cx)))
                .flex_1()
                .min_h_0()
                .max_w(px(480.)),
            )
    }
}
