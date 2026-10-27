use super::{Block, Document};
use crate::theme::ActiveTheme;
use gpui::{
    AnyElement, App, Bounds, ClipboardItem, DispatchPhase, Element, ElementId, Entity, FocusHandle,
    Global, GlobalElementId, Hitbox, HitboxBehavior, InspectorElementId, IntoElement, KeyBinding,
    LayoutId, MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, Point, SharedString, StyledText,
    TextAlign, TextLayout, TextRun, Window, div, prelude::*, px,
};
use std::{ops::Range, rc::Rc, sync::Arc};
use unicode_segmentation::UnicodeSegmentation;

pub(super) type StartHandler = Rc<dyn Fn(&mut Window, &mut App) -> Option<FocusHandle>>;

gpui::actions!(
    markdown_selection,
    [
        Copy,
        SelectAll,
        Clear,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        Up,
        Down,
        SelectUp,
        SelectDown,
        Home,
        End,
        SelectHome,
        SelectEnd,
        First,
        Last,
        SelectFirst,
        SelectLast
    ]
);

struct Bindings;
impl Global for Bindings {}

fn bind_keys(cx: &mut App) {
    if cx.has_global::<Bindings>() {
        return;
    }
    cx.set_global(Bindings);
    cx.bind_keys([
        KeyBinding::new("secondary-c", Copy, Some("MarkdownText && selection")),
        KeyBinding::new("cmd-c", Copy, Some("MarkdownText && selection")),
        KeyBinding::new("secondary-a", SelectAll, Some("MarkdownText")),
        KeyBinding::new("cmd-a", SelectAll, Some("MarkdownText")),
        KeyBinding::new("escape", Clear, Some("MarkdownText && selection")),
        KeyBinding::new("left", Left, Some("MarkdownText && selection")),
        KeyBinding::new("right", Right, Some("MarkdownText && selection")),
        KeyBinding::new("shift-left", SelectLeft, Some("MarkdownText")),
        KeyBinding::new("shift-right", SelectRight, Some("MarkdownText")),
        KeyBinding::new("up", Up, Some("MarkdownText && selection")),
        KeyBinding::new("down", Down, Some("MarkdownText && selection")),
        KeyBinding::new("shift-up", SelectUp, Some("MarkdownText")),
        KeyBinding::new("shift-down", SelectDown, Some("MarkdownText")),
        KeyBinding::new("home", Home, Some("MarkdownText && selection")),
        KeyBinding::new("end", End, Some("MarkdownText && selection")),
        KeyBinding::new("shift-home", SelectHome, Some("MarkdownText")),
        KeyBinding::new("shift-end", SelectEnd, Some("MarkdownText")),
        KeyBinding::new("secondary-home", First, Some("MarkdownText && selection")),
        KeyBinding::new("secondary-end", Last, Some("MarkdownText && selection")),
        KeyBinding::new("secondary-shift-home", SelectFirst, Some("MarkdownText")),
        KeyBinding::new("secondary-shift-end", SelectLast, Some("MarkdownText")),
    ]);
    // Mac keyboards have no Home or End key; Command and an arrow stand in for them.
    #[cfg(target_os = "macos")]
    cx.bind_keys([
        KeyBinding::new("cmd-left", Home, Some("MarkdownText && selection")),
        KeyBinding::new("cmd-right", End, Some("MarkdownText && selection")),
        KeyBinding::new("cmd-shift-left", SelectHome, Some("MarkdownText")),
        KeyBinding::new("cmd-shift-right", SelectEnd, Some("MarkdownText")),
        KeyBinding::new("cmd-up", First, Some("MarkdownText && selection")),
        KeyBinding::new("cmd-down", Last, Some("MarkdownText && selection")),
        KeyBinding::new("cmd-shift-up", SelectFirst, Some("MarkdownText")),
        KeyBinding::new("cmd-shift-down", SelectLast, Some("MarkdownText")),
    ]);
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Position {
    run: usize,
    byte: usize,
}

struct Press {
    origin: Point<Pixels>,
    active: bool,
    dragged: bool,
    open_link: bool,
}

pub(super) struct Selection {
    document: Arc<Document>,
    texts: Vec<SharedString>,
    links: Vec<Vec<(Range<usize>, SharedString)>>,
    layouts: Vec<Option<Layout>>,
    focus: Option<FocusHandle>,
    own_focus: Option<FocusHandle>,
    anchor: Position,
    head: Position,
    press: Option<Press>,
    goal_x: Option<Pixels>,
    reveal_head: bool,
    head_after_wrap: bool,
}

#[derive(Clone)]
struct Layout {
    text: TextLayout,
    align: TextAlign,
}

impl Layout {
    fn bounds(&self) -> Bounds<Pixels> {
        self.text.bounds()
    }

    fn line_height(&self) -> Pixels {
        self.text.line_height()
    }

    fn alignment_offset(&self, y: Pixels) -> Pixels {
        if self.align == TextAlign::Left {
            return px(0.);
        }
        let mut row = ((y - self.bounds().top()) / self.line_height())
            .floor()
            .max(0.) as usize;
        let lines = self.text.line_layouts();
        for (index, line) in lines.iter().enumerate() {
            if row > line.wrap_boundaries.len() && index + 1 < lines.len() {
                row -= line.wrap_boundaries.len() + 1;
                continue;
            }
            let row = row.min(line.wrap_boundaries.len());
            let boundary_x = |index: usize| {
                let boundary = &line.wrap_boundaries[index];
                line.unwrapped_layout.runs[boundary.run_ix].glyphs[boundary.glyph_ix]
                    .position
                    .x
            };
            let start = if row == 0 {
                px(0.)
            } else {
                boundary_x(row - 1)
            };
            let end = if row == line.wrap_boundaries.len() {
                line.unwrapped_layout.width
            } else {
                boundary_x(row)
            };
            let extra = self.bounds().size.width - (end - start);
            return if self.align == TextAlign::Center {
                extra / 2.
            } else {
                extra
            };
        }
        px(0.)
    }

    fn index_for_position(&self, mut at: Point<Pixels>) -> Result<usize, usize> {
        at.x -= self.alignment_offset(at.y);
        self.text.index_for_position(at)
    }

    fn position_for_index(&self, index: usize, after_wrap: bool) -> Option<Point<Pixels>> {
        let mut at = self.text.position_for_index(index)?;
        if after_wrap {
            let mut start = 0;
            let mut y = self.bounds().top();
            for line in self.text.line_layouts() {
                if let Some(row) = line.wrap_boundaries.iter().position(|boundary| {
                    start
                        + line.unwrapped_layout.runs[boundary.run_ix].glyphs[boundary.glyph_ix]
                            .index
                        == index
                }) {
                    at = gpui::point(
                        self.bounds().left(),
                        y + self.line_height() * (row + 1) as f32,
                    );
                    break;
                }
                start += line.len() + 1;
                y += self.line_height() * (line.wrap_boundaries.len() + 1) as f32;
            }
        }
        at.x += self.alignment_offset(at.y);
        Some(at)
    }
}

fn visit(block: &Block, visitor: &mut impl FnMut(&SharedString, &[(Range<usize>, SharedString)])) {
    match block {
        Block::Paragraph(text) | Block::Heading { text, .. } => visitor(&text.text, &text.links),
        Block::Code { code, .. } => visitor(code, &[]),
        Block::List { items, .. } => {
            for block in items.iter().flatten() {
                visit(block, visitor);
            }
        }
        Block::Quote(blocks) => {
            for block in blocks {
                visit(block, visitor);
            }
        }
        Block::Table { header, rows, .. } => {
            for text in header.iter().chain(rows.iter().flatten()) {
                visitor(&text.text, &text.links);
            }
        }
        Block::Diagram { .. } | Block::Rule | Block::Images(_) => {}
    }
}

pub(super) fn has_selectable_text(document: &Document) -> bool {
    let mut found = false;
    for top in &document.blocks {
        visit(&top.block, &mut |text, _| found |= !text.is_empty());
        if found {
            break;
        }
    }
    found
}

impl Selection {
    pub fn prepare(
        id: &SharedString,
        document: &Arc<Document>,
        focus: Option<FocusHandle>,
        managed: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        bind_keys(cx);
        let state = window.use_keyed_state(
            ElementId::Name(format!("{id}-selection").into()),
            cx,
            |_, cx| {
                let focus = (!managed).then(|| cx.focus_handle());
                Self {
                    document: Arc::new(Document::default()),
                    texts: Vec::new(),
                    links: Vec::new(),
                    layouts: Vec::new(),
                    focus: focus.clone(),
                    own_focus: focus,
                    anchor: Position::default(),
                    head: Position::default(),
                    press: None,
                    goal_x: None,
                    reveal_head: false,
                    head_after_wrap: false,
                }
            },
        );
        state.update(cx, |state, _| {
            state.focus = focus.or_else(|| state.own_focus.clone());
            if !state.is_focused(window) {
                state.anchor = Position::default();
                state.head = Position::default();
                state.press = None;
                state.goal_x = None;
                state.reveal_head = false;
                state.head_after_wrap = false;
            }
            if !Arc::ptr_eq(&state.document, document) && state.document != *document {
                let mut texts = Vec::new();
                state.links.clear();
                for top in &document.blocks {
                    visit(&top.block, &mut |text, links| {
                        texts.push(text.clone());
                        state.links.push(links.to_vec());
                    });
                }
                let append_only = texts.len() >= state.texts.len()
                    && state
                        .texts
                        .iter()
                        .zip(&texts)
                        .enumerate()
                        .all(|(index, (old, new))| {
                            if index + 1 == state.texts.len() {
                                new.starts_with(old.as_ref())
                            } else {
                                new == old
                            }
                        });
                let valid_endpoints = [state.anchor, state.head].into_iter().all(|position| {
                    texts.get(position.run).is_some_and(|text| {
                        position.byte == text.len()
                            || text
                                .grapheme_indices(true)
                                .any(|(at, _)| at == position.byte)
                    })
                });
                if !append_only || !valid_endpoints {
                    state.anchor = Position::default();
                    state.head = Position::default();
                    state.press = None;
                    state.goal_x = None;
                    state.reveal_head = false;
                    state.head_after_wrap = false;
                }
                state.texts = texts;
            }
            state.document = document.clone();
            state.layouts = vec![None; state.texts.len()];
        });
        state
    }

    fn range(&self) -> Range<Position> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }

    fn is_focused(&self, window: &Window) -> bool {
        self.focus
            .as_ref()
            .is_some_and(|focus| focus.is_focused(window))
    }

    fn selected_text(&self) -> String {
        let range = self.range();
        if range.is_empty() || self.texts.is_empty() {
            return String::new();
        }
        (range.start.run..=range.end.run)
            .map(|run| {
                let text = &self.texts[run];
                let start = if run == range.start.run {
                    range.start.byte
                } else {
                    0
                };
                let end = if run == range.end.run {
                    range.end.byte
                } else {
                    text.len()
                };
                &text[start..end]
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn hit(&self, at: Point<Pixels>, strict: bool) -> Option<Position> {
        let (run, _) = self
            .layouts
            .iter()
            .enumerate()
            .filter_map(|(run, layout)| layout.as_ref().map(|layout| (run, layout)))
            .filter(|(_, layout)| !strict || layout.bounds().contains(&at))
            .min_by(|(_, a), (_, b)| {
                let distance = |layout: &Layout| {
                    let b = layout.bounds();
                    let x = (b.left() - at.x).max(px(0.)).max(at.x - b.right());
                    let y = (b.top() - at.y).max(px(0.)).max(at.y - b.bottom());
                    f32::from(x).powi(2) + f32::from(y).powi(2)
                };
                distance(a)
                    .partial_cmp(&distance(b))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })?;
        self.position_in(run, at, true)
    }

    fn position_in(&self, run: usize, at: Point<Pixels>, nearest: bool) -> Option<Position> {
        let hit = self.layouts.get(run)?.as_ref()?.index_for_position(at);
        let byte = if nearest {
            hit.unwrap_or_else(|byte| byte)
        } else {
            hit.ok()?
        }
        .min(self.texts[run].len());
        let byte = self.texts[run]
            .grapheme_indices(true)
            .map(|(at, _)| at)
            .chain(std::iter::once(self.texts[run].len()))
            .take_while(|at| *at <= byte)
            .last()
            .unwrap_or(0);
        Some(Position { run, byte })
    }

    fn after_wrap(&self, position: Position, at: Point<Pixels>) -> bool {
        let Some(layout) = self.layouts[position.run].as_ref() else {
            return false;
        };
        let height = layout.line_height();
        if height <= px(0.) || layout.bounds().size.height <= px(0.) {
            return false;
        }
        let row = ((at.y - layout.bounds().top()) / height)
            .floor()
            .max(0.)
            .min((layout.bounds().size.height / height).ceil() - 1.);
        let y = layout.bounds().top() + height * row;
        layout
            .position_for_index(position.byte, false)
            .is_some_and(|position| position.y < y - px(0.5))
    }

    fn pointer_move(&mut self, at: Point<Pixels>) {
        let Some(press) = self.press.as_mut().filter(|press| press.active) else {
            return;
        };
        press.dragged |= (at - press.origin).magnitude() >= 3.;
        if press.dragged
            && let Some(position) = self.hit(at, false)
        {
            self.head = position;
            self.head_after_wrap = self.after_wrap(position, at);
        }
    }

    fn clicked_link(&self, at: Point<Pixels>) -> Option<SharedString> {
        let press = self.press.as_ref()?;
        if press.dragged || !press.open_link || (at - press.origin).magnitude() >= 3. {
            return None;
        }
        let start = self.position_in(self.hit(press.origin, true)?.run, press.origin, false)?;
        let end = self.position_in(self.hit(at, true)?.run, at, false)?;
        if start.run != end.run {
            return None;
        }
        self.links[start.run]
            .iter()
            .find(|(range, url)| {
                range.contains(&start.byte) && range.contains(&end.byte) && super::is_openable(url)
            })
            .map(|(_, url)| url.clone())
    }

    fn step(&self, forward: bool) -> Position {
        let Position { run, byte } = self.head;
        let text = &self.texts[run];
        if forward {
            text.grapheme_indices(true)
                .map(|(at, _)| at)
                .chain(std::iter::once(text.len()))
                .find(|at| *at > byte)
                .map(|byte| Position { run, byte })
                .unwrap_or_else(|| {
                    if run + 1 < self.texts.len() {
                        Position {
                            run: run + 1,
                            byte: 0,
                        }
                    } else {
                        self.head
                    }
                })
        } else {
            text.grapheme_indices(true)
                .map(|(at, _)| at)
                .take_while(|at| *at < byte)
                .last()
                .map(|byte| Position { run, byte })
                .unwrap_or_else(|| {
                    if run > 0 {
                        Position {
                            run: run - 1,
                            byte: self.texts[run - 1].len(),
                        }
                    } else {
                        self.head
                    }
                })
        }
    }

    fn navigate(&mut self, motion: Motion, extend: bool) {
        if self.texts.is_empty() {
            return;
        }
        let range = self.range();
        let next = match motion {
            Motion::Left if !extend && !range.is_empty() => range.start,
            Motion::Right if !extend && !range.is_empty() => range.end,
            Motion::Left => self.step(false),
            Motion::Right => self.step(true),
            Motion::First => Position::default(),
            Motion::Last => Position {
                run: self.texts.len() - 1,
                byte: self.texts.last().unwrap().len(),
            },
            Motion::Up | Motion::Down => {
                let Some(layout) = self.layouts[self.head.run].as_ref() else {
                    return;
                };
                let Some(at) = layout.position_for_index(self.head.byte, self.head_after_wrap)
                else {
                    return;
                };
                let goal = *self.goal_x.get_or_insert(at.x);
                let down = matches!(motion, Motion::Down);
                let next = self
                    .layouts
                    .iter()
                    .enumerate()
                    .filter_map(|(run, layout)| {
                        let layout = layout.as_ref()?;
                        let bounds = layout.bounds();
                        let height = layout.line_height();
                        if height <= px(0.) || bounds.size.height <= px(0.) {
                            return None;
                        }
                        let last = (bounds.size.height / height).ceil() - 1.;
                        let offset = (at.y - bounds.top()) / height;
                        let row = if down {
                            offset.floor() + 1.
                        } else {
                            offset.ceil() - 1.
                        }
                        .max(0.)
                        .min(last);
                        let y = bounds.top() + height * row;
                        if (down && y <= at.y) || (!down && y >= at.y) {
                            return None;
                        }
                        let x_distance = (bounds.left() - goal)
                            .max(px(0.))
                            .max(goal - bounds.right());
                        Some((
                            run,
                            gpui::point(goal, y + height / 2.),
                            f32::from(y - at.y).abs(),
                            f32::from(x_distance),
                        ))
                    })
                    .min_by(|a, b| a.2.total_cmp(&b.2).then_with(|| a.3.total_cmp(&b.3)));
                if let Some((run, at, _, _)) = next {
                    if let Some(next) = self.position_in(run, at, true) {
                        self.head_after_wrap = self.after_wrap(next, at);
                        next
                    } else {
                        self.head
                    }
                } else {
                    self.head
                }
            }
            Motion::Home | Motion::End => {
                let Some(layout) = self.layouts[self.head.run].as_ref() else {
                    return;
                };
                let Some(mut at) = layout.position_for_index(self.head.byte, self.head_after_wrap)
                else {
                    return;
                };
                let height = layout.line_height();
                match motion {
                    Motion::Home => at.x = layout.bounds().left() - px(1.),
                    Motion::End => at.x = layout.bounds().right() + px(1.),
                    _ => unreachable!(),
                }
                at.y += height / 2.;
                if let Some(next) = self.hit(at, false) {
                    self.head_after_wrap = self.after_wrap(next, at);
                    next
                } else {
                    self.head
                }
            }
        };
        if !matches!(motion, Motion::Up | Motion::Down) {
            self.goal_x = None;
            if !matches!(motion, Motion::Home | Motion::End) {
                self.head_after_wrap = false;
            }
        }
        self.head = next;
        if !extend {
            self.anchor = next;
        }
        self.press = None;
    }
}

#[derive(Clone, Copy)]
enum Motion {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    First,
    Last,
}

pub(super) fn text(
    value: SharedString,
    runs: Vec<TextRun>,
    index: usize,
    state: &Entity<Selection>,
    cx: &App,
) -> StyledText {
    let range = state.read(cx).range();
    let start = if index == range.start.run {
        range.start.byte
    } else {
        0
    };
    let end = if index == range.end.run {
        range.end.byte
    } else {
        value.len()
    };
    if range.is_empty() || index < range.start.run || index > range.end.run || start >= end {
        return StyledText::new(value).with_runs(runs);
    }
    let mut selected = Vec::new();
    let mut at = 0;
    for run in runs {
        let limit = at + run.len;
        let mut cuts = vec![at, limit];
        cuts.extend(
            [start, end]
                .into_iter()
                .filter(|cut| at < *cut && *cut < limit),
        );
        cuts.sort_unstable();
        for pair in cuts.windows(2) {
            let mut piece = run.clone();
            piece.len = pair[1] - pair[0];
            if pair[0] >= start && pair[1] <= end {
                piece.background_color = Some(cx.theme().colors.selection);
            }
            selected.push(piece);
        }
        at = limit;
    }
    StyledText::new(value).with_runs(selected)
}

pub(super) fn track(
    child: AnyElement,
    layout: TextLayout,
    index: usize,
    state: Entity<Selection>,
) -> AnyElement {
    TrackedText {
        child,
        layout,
        index,
        state,
    }
    .into_any_element()
}

struct TrackedText {
    child: AnyElement,
    layout: TextLayout,
    index: usize,
    state: Entity<Selection>,
}

impl IntoElement for TrackedText {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for TrackedText {
    type RequestLayoutState = ();
    type PrepaintState = Hitbox;
    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Hitbox {
        self.child.prepaint(window, cx);
        self.state.update(cx, |state, _| {
            state.layouts[self.index] = Some(Layout {
                text: self.layout.clone(),
                align: window.text_style().text_align,
            })
        });
        window.insert_hitbox(bounds, HitboxBehavior::Normal)
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        hitbox: &mut Hitbox,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
        if let Some(layout) = &self.state.read(cx).layouts[self.index]
            && let Ok(byte) = layout.index_for_position(window.mouse_position())
            && self.state.read(cx).links[self.index]
                .iter()
                .any(|(range, _)| range.contains(&byte))
        {
            window.set_cursor_style(gpui::CursorStyle::PointingHand, hitbox);
        }
    }
}

pub(super) fn surface(
    id: SharedString,
    child: impl IntoElement,
    state: Entity<Selection>,
    on_start: Option<StartHandler>,
    cx: &App,
) -> impl IntoElement {
    let focus = state.read(cx).focus.clone();
    let context = if state.read(cx).range().is_empty() {
        "MarkdownText"
    } else {
        "MarkdownText selection"
    };
    let mut root = crate::automation::tag(
        div()
            .id(ElementId::Name(format!("{id}-selection-surface").into()))
            .min_w_0()
            .w_full()
            .when_some(focus.as_ref(), |d, focus| d.track_focus(focus))
            .role(gpui::Role::Group)
            .aria_label("Select reply text")
            .cursor(gpui::CursorStyle::IBeam),
        format!("{id}-selection-surface"),
    )
    .key_context(context)
    .on_mouse_down(MouseButton::Left, {
        let state = state.clone();
        move |event, window, cx| {
            let position = state.read(cx).hit(event.position, true);
            if position.is_some() {
                window.prevent_default();
            }
            let focus = if position.is_some()
                && let Some(start) = &on_start
            {
                let Some(focus) = start(window, cx) else {
                    state.update(cx, |state, _| state.press = None);
                    return;
                };
                Some(focus)
            } else {
                state.read(cx).focus.clone()
            };
            if position.is_some() {
                cx.stop_propagation();
            }
            state.update(cx, |state, cx| {
                state.press = None;
                if let Some(position) = position {
                    if !event.modifiers.shift {
                        state.anchor = position;
                    }
                    state.head = position;
                    state.goal_x = None;
                    state.reveal_head = false;
                    state.head_after_wrap = state.after_wrap(position, event.position);
                    state.press = Some(Press {
                        origin: event.position,
                        active: true,
                        dragged: false,
                        open_link: !event.modifiers.shift,
                    });
                    state.focus = focus;
                    if let Some(focus) = &state.focus {
                        window.focus(focus, cx);
                    }
                    cx.notify();
                } else if state.anchor != state.head {
                    state.anchor = state.head;
                    cx.notify();
                }
            })
        }
    })
    .on_mouse_down_out({
        let state = state.clone();
        move |_, _, cx| {
            state.update(cx, |state, cx| {
                state.press = None;
                if state.anchor != state.head {
                    state.anchor = state.head;
                    cx.notify();
                }
            })
        }
    })
    .on_action({
        let state = state.clone();
        move |_: &Copy, window, cx| {
            let state = state.read(cx);
            if !state.is_focused(window) || state.range().is_empty() {
                cx.propagate();
                return;
            }
            cx.write_to_clipboard(ClipboardItem::new_string(state.selected_text()));
        }
    })
    .on_action({
        let state = state.clone();
        move |_: &SelectAll, window, cx| {
            state.update(cx, |state, cx| {
                if !state.is_focused(window) {
                    cx.propagate();
                    return;
                }
                state.navigate(Motion::First, false);
                state.navigate(Motion::Last, true);
                state.reveal_head = false;
                cx.notify();
            })
        }
    })
    .on_action({
        let state = state.clone();
        move |_: &Clear, window, cx| {
            state.update(cx, |state, cx| {
                if !state.is_focused(window) {
                    cx.propagate();
                    return;
                }
                state.anchor = state.head;
                state.press = None;
                state.reveal_head = false;
                cx.notify();
            })
        }
    });
    macro_rules! navigation {
        ($action:ty, $motion:ident, $extend:expr) => {
            root = root.on_action({
                let state = state.clone();
                move |_: &$action, window, cx| {
                    state.update(cx, |state, cx| {
                        if !state.is_focused(window) {
                            cx.propagate();
                            return;
                        }
                        state.navigate(Motion::$motion, $extend);
                        state.reveal_head = true;
                        cx.notify();
                    })
                }
            });
        };
    }
    navigation!(Left, Left, false);
    navigation!(Right, Right, false);
    navigation!(SelectLeft, Left, true);
    navigation!(SelectRight, Right, true);
    navigation!(Up, Up, false);
    navigation!(Down, Down, false);
    navigation!(SelectUp, Up, true);
    navigation!(SelectDown, Down, true);
    navigation!(Home, Home, false);
    navigation!(End, End, false);
    navigation!(SelectHome, Home, true);
    navigation!(SelectEnd, End, true);
    navigation!(First, First, false);
    navigation!(Last, Last, false);
    navigation!(SelectFirst, First, true);
    navigation!(SelectLast, Last, true);
    Surface {
        child: root.child(child).into_any_element(),
        state,
    }
}

struct Surface {
    child: AnyElement,
    state: Entity<Selection>,
}
impl IntoElement for Surface {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
impl Element for Surface {
    type RequestLayoutState = ();
    type PrepaintState = ();
    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.prepaint(window, cx);
        let head = self.state.update(cx, |state, _| {
            if !std::mem::take(&mut state.reveal_head) || !state.is_focused(window) {
                return None;
            }
            let layout = state.layouts.get(state.head.run)?.as_ref()?;
            let position = layout.position_for_index(state.head.byte, state.head_after_wrap)?;
            Some(Bounds::new(
                position,
                gpui::size(px(2.), layout.line_height()),
            ))
        });
        if let Some(head) = head {
            window.request_autoscroll(head);
        }
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
        if self.state.read(cx).is_focused(window) && window.last_input_was_keyboard() {
            window.paint_quad(gpui::outline(
                bounds,
                cx.theme().colors.border_focused,
                gpui::BorderStyle::default(),
            ));
        }
        window.on_mouse_event({
            let state = self.state.clone();
            move |event: &MouseMoveEvent, phase, _, cx| {
                if phase != DispatchPhase::Capture {
                    return;
                }
                state.update(cx, |state, cx| {
                    if state.press.as_ref().is_some_and(|press| press.active) {
                        if event.pressed_button != Some(MouseButton::Left) {
                            state.press = None;
                        } else {
                            state.pointer_move(event.position);
                        }
                        cx.notify();
                    }
                });
            }
        });
        window.on_mouse_event({
            let state = self.state.clone();
            move |event: &MouseUpEvent, phase, _, cx| {
                if phase != DispatchPhase::Capture || event.button != MouseButton::Left {
                    return;
                }
                state.update(cx, |state, cx| {
                    if state.press.as_ref().is_some_and(|press| press.active) {
                        state.pointer_move(event.position);
                        let link = state.clicked_link(event.position);
                        let press = state.press.as_mut().unwrap();
                        press.active = false;
                        if let Some(url) = link {
                            cx.open_url(&url);
                        }
                        cx.notify();
                    }
                });
            }
        });
    }
}

#[cfg(all(test, feature = "ui-tests"))]
mod tests {
    #[gpui::test]
    fn markdown_selection_soft_wrap_home_end_preserve_visual_row(cx: &mut TestAppContext) {
        let (_view, cx) = reply(cx, &"alpha **bold** beta ".repeat(12));
        let start = cx.debug_bounds("reply-bounds").unwrap().origin + point(px(0.), px(8.));
        cx.simulate_click(start, Modifiers::none());
        cx.simulate_keystrokes("shift-end secondary-c");
        let first = clipboard(cx).unwrap();
        cx.simulate_keystrokes("shift-home secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some("whole row"));
        cx.simulate_keystrokes("shift-down shift-end secondary-c");
        let second = clipboard(cx).unwrap();
        assert!(second.len() > first.len());
        cx.simulate_keystrokes("shift-home secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some(first.as_str()));
        cx.simulate_keystrokes("shift-down secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some(second.as_str()));
    }

    #[gpui::test]
    fn markdown_selection_vertical_moves_keep_table_column(cx: &mut TestAppContext) {
        let (_view, cx) = reply(cx, "| left | right |\n| -- | -- |\n| next | other |");
        let bounds = cx.debug_bounds("reply-bounds").unwrap();
        cx.simulate_click(bounds.origin + point(px(14.), px(12.)), Modifiers::none());
        cx.simulate_keystrokes("shift-down secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some("left\nright\n"));
        cx.simulate_keystrokes("shift-up secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some("whole row"));
        cx.simulate_click(bounds.origin + point(px(174.), px(12.)), Modifiers::none());
        cx.simulate_keystrokes("shift-down secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some("right\nnext\n"));
    }

    #[gpui::test]
    fn markdown_selection_requires_glyph_hits_for_links(cx: &mut TestAppContext) {
        let (view, cx) = reply(cx, "| [ab](https://example.com/cell) |\n| --: |\n| x |");
        let bounds = cx.debug_bounds("reply-bounds").unwrap();
        cx.simulate_click(
            point(bounds.left() + px(30.), bounds.top() + px(12.)),
            Modifiers::none(),
        );
        assert_eq!(
            cx.opened_url(),
            None,
            "right-aligned cell whitespace is not a link"
        );
        view.update(cx, |view, cx| {
            view.source = "| [ab](https://example.com/center) |\n| :-: |\n| x |".into();
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_click(
            point(bounds.left() + px(30.), bounds.top() + px(12.)),
            Modifiers::none(),
        );
        assert_eq!(
            cx.opened_url(),
            None,
            "centered cell whitespace is not a link"
        );
        view.update(cx, |view, cx| {
            view.source = format!(
                "[{}](https://example.com/wrap)",
                "wordy link text ".repeat(8).trim_end()
            );
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_click(
            point(bounds.right() - px(2.), bounds.top() + px(8.)),
            Modifiers::none(),
        );
        assert_eq!(
            cx.opened_url(),
            None,
            "wrapped-line trailing whitespace is not a link"
        );
        cx.simulate_click(
            point(bounds.left() + px(2.), bounds.top() + px(8.)),
            Modifiers::none(),
        );
        assert_eq!(cx.opened_url().as_deref(), Some("https://example.com/wrap"));
    }

    #[gpui::test]
    fn markdown_selection_vertical_moves_cross_nontext_gaps(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (_view, cx) = reply(
            cx,
            "para one\n\n```rs\ncode line\n```\n\n```mermaid\nflowchart TD\nA-->B\n```\n\ntail",
        );
        let start = cx.debug_bounds("reply-bounds").unwrap().origin + point(px(0.), px(8.));
        cx.simulate_click(start, Modifiers::none());
        cx.simulate_keystrokes("shift-down secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some("para one\n"));
        cx.simulate_keystrokes("shift-down secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some("para one\ncode line\n"));
        cx.simulate_keystrokes("shift-end secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some("para one\ncode line\ntail"));
        cx.simulate_keystrokes("right shift-home secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some("tail"));
        cx.simulate_keystrokes("shift-up secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some("code line\ntail"));
        cx.simulate_keystrokes("shift-up secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some("para one\ncode line\ntail"));
    }

    use crate::Markdown;
    use gpui::{
        ClipboardItem, Context, FocusHandle, IntoElement, KeyBinding, Modifiers, Render,
        TestAppContext, Window, div, point, prelude::*, px,
    };

    gpui::actions!(markdown_selection_test, [CopyRow, NavigateRow]);

    struct Reply {
        source: String,
        focus: FocusHandle,
        text_focus: FocusHandle,
        navigations: usize,
        reject_pointer: bool,
    }

    impl Render for Reply {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let focus = self.text_focus.clone();
            let reject = self.reject_pointer;
            div()
                .size_full()
                .track_focus(&self.focus)
                .key_context("SelectionTestTranscript")
                .on_action(|_: &CopyRow, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string("whole row".into()));
                })
                .on_action(cx.listener(|this, _: &NavigateRow, _, cx| {
                    this.navigations += 1;
                    cx.notify();
                }))
                .child(
                    div()
                        .w(px(320.))
                        .debug_selector(|| "reply-bounds".into())
                        .child(
                            Markdown::from_source("reply", &self.source)
                                .managed_focus()
                                .selection_focus(&self.text_focus)
                                .on_selection_start(move |_, _| (!reject).then(|| focus.clone())),
                        ),
                )
        }
    }

    fn reply<'a>(
        cx: &'a mut TestAppContext,
        source: &str,
    ) -> (gpui::Entity<Reply>, &'a mut gpui::VisualTestContext) {
        cx.update(|cx| {
            crate::init(cx);
            cx.bind_keys([
                KeyBinding::new("secondary-c", CopyRow, Some("SelectionTestTranscript")),
                KeyBinding::new("home", NavigateRow, Some("SelectionTestTranscript")),
                KeyBinding::new("end", NavigateRow, Some("SelectionTestTranscript")),
                KeyBinding::new("up", NavigateRow, Some("SelectionTestTranscript")),
                KeyBinding::new("down", NavigateRow, Some("SelectionTestTranscript")),
                KeyBinding::new("left", NavigateRow, Some("SelectionTestTranscript")),
                KeyBinding::new("right", NavigateRow, Some("SelectionTestTranscript")),
            ]);
        });
        let (view, cx) = cx.add_window_view(|window, cx| {
            let focus = cx.focus_handle();
            window.focus(&focus, cx);
            Reply {
                source: source.into(),
                focus,
                text_focus: cx.focus_handle().tab_stop(false),
                navigations: 0,
                reject_pointer: false,
            }
        });
        cx.run_until_parked();
        (view, cx)
    }

    fn clipboard(cx: &mut gpui::VisualTestContext) -> Option<String> {
        cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()))
    }

    #[gpui::test]
    fn markdown_selection_drag_copies_rendered_styles(cx: &mut TestAppContext) {
        let (_view, cx) = reply(cx, "alpha **bold** *italic* `code`.");
        let bounds = cx.debug_bounds("reply-bounds").unwrap();
        let start = bounds.origin + point(px(0.), px(8.));
        let end = start + point(px(310.), px(0.));
        cx.simulate_mouse_down(start, gpui::MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(end, gpui::MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(end, gpui::MouseButton::Left, Modifiers::none());
        cx.simulate_keystrokes("secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some("alpha bold italic code."));
    }

    #[gpui::test]
    fn markdown_selection_keyboard_overrides_copy_only_with_range(cx: &mut TestAppContext) {
        let (_view, cx) = reply(cx, "**alpha** `beta`");
        let bounds = cx.debug_bounds("reply-bounds").unwrap();
        cx.simulate_click(bounds.origin + point(px(1.), px(8.)), Modifiers::none());
        cx.simulate_keystrokes("secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some("whole row"));
        cx.simulate_keystrokes("home shift-right shift-right shift-right secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some("alp"));
        cx.simulate_keystrokes("escape secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some("whole row"));
    }

    #[gpui::test]
    fn markdown_selection_links_click_but_never_open_after_drag(cx: &mut TestAppContext) {
        let (_view, cx) = reply(cx, "[a **long** link](https://example.com)");
        let bounds = cx.debug_bounds("reply-bounds").unwrap();
        let start = bounds.origin + point(px(1.), px(8.));
        for end in [
            start + point(px(40.), px(0.)),
            start + point(px(500.), px(100.)),
        ] {
            cx.simulate_mouse_down(start, gpui::MouseButton::Left, Modifiers::none());
            cx.simulate_mouse_move(end, gpui::MouseButton::Left, Modifiers::none());
            cx.simulate_mouse_move(start, gpui::MouseButton::Left, Modifiers::none());
            cx.simulate_mouse_up(start, gpui::MouseButton::Left, Modifiers::none());
            assert_eq!(cx.opened_url(), None);
        }
        cx.simulate_mouse_down(start, gpui::MouseButton::Right, Modifiers::none());
        cx.simulate_mouse_up(start, gpui::MouseButton::Right, Modifiers::none());
        assert_eq!(cx.opened_url(), None);
        cx.simulate_click(start, Modifiers::none());
        assert_eq!(cx.opened_url().as_deref(), Some("https://example.com"));
    }

    #[gpui::test]
    fn markdown_selection_graphemes_reverse_ranges_and_streaming(cx: &mut TestAppContext) {
        let (view, cx) = reply(cx, "e\u{301}👩‍💻**界** tail");
        let start = cx.debug_bounds("reply-bounds").unwrap().origin + point(px(0.), px(8.));
        cx.simulate_click(start, Modifiers::none());
        cx.simulate_keystrokes("shift-right shift-right secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some("e\u{301}👩‍💻"));
        view.update(cx, |view, cx| {
            view.source.push_str(" appended\n\nnext block");
            cx.notify();
        });
        cx.simulate_keystrokes("secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some("e\u{301}👩‍💻"));
        cx.simulate_keystrokes("right shift-left secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some("👩‍💻"));
        view.update(cx, |view, cx| {
            view.source = "replacement".into();
            cx.notify();
        });
        cx.simulate_keystrokes("secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some("whole row"));
    }

    #[gpui::test]
    fn markdown_selection_all_spans_nested_blocks_and_code(cx: &mut TestAppContext) {
        let (_view, cx) = reply(
            cx,
            "# Title\n\n- a **bold**\n- é\n\n| H | J |\n| -- | -- |\n| x | y |\n\n```rs\nlet x = 1;\n```",
        );
        let start = cx.debug_bounds("reply-bounds").unwrap().origin + point(px(0.), px(8.));
        cx.simulate_click(start, Modifiers::none());
        cx.simulate_keystrokes("secondary-a secondary-c");
        assert_eq!(
            clipboard(cx).as_deref(),
            Some("Title\na bold\né\nH\nJ\nx\ny\nlet x = 1;")
        );
        let copy = cx.debug_bounds("reply-copy-1").unwrap();
        cx.simulate_click(copy.center(), Modifiers::none());
        assert_eq!(clipboard(cx).as_deref(), Some("let x = 1;"));
    }

    #[gpui::test]
    fn markdown_selection_drag_release_outside_and_reverse(cx: &mut TestAppContext) {
        let (_view, cx) = reply(cx, "alpha **bold** *italic* `code`.");
        let bounds = cx.debug_bounds("reply-bounds").unwrap();
        let start = bounds.origin + point(px(0.), px(8.));
        let end = start + point(px(500.), px(100.));
        cx.simulate_mouse_down(start, gpui::MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(end, gpui::MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(end, gpui::MouseButton::Left, Modifiers::none());
        cx.simulate_keystrokes("secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some("alpha bold italic code."));
        cx.simulate_keystrokes("right shift-home secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some("alpha bold italic code."));
    }

    #[test]
    fn markdown_selection_only_reports_useful_text_surfaces() {
        use crate::markdown::Document;
        for source in [
            "",
            "---",
            "```mermaid\ngraph TD; A-->B\n```",
            "````\n\n````",
        ] {
            assert!(!Document::parse(source).has_selectable_text(), "{source}");
        }
        for source in [
            "text",
            "- nested",
            "> quote",
            "```rs\ncode\n```",
            "| h |\n| - |\n| x |",
        ] {
            assert!(Document::parse(source).has_selectable_text(), "{source}");
        }
    }

    #[gpui::test]
    fn markdown_selection_hits_right_aligned_table_glyphs(cx: &mut TestAppContext) {
        let (_view, cx) = reply(cx, "| abc |\n| --: |\n| def |");
        let bounds = cx.debug_bounds("reply-bounds").unwrap();
        cx.simulate_click(
            point(bounds.right() - px(15.), bounds.top() + px(12.)),
            Modifiers::none(),
        );
        cx.simulate_keystrokes("shift-right secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some("c"));
    }

    #[gpui::test]
    fn markdown_selection_empty_ranges_preserve_transcript_navigation(cx: &mut TestAppContext) {
        let (view, cx) = reply(cx, "alpha **bold**");
        let start = cx.debug_bounds("reply-bounds").unwrap().origin + point(px(0.), px(8.));
        cx.simulate_click(start, Modifiers::none());
        cx.simulate_keystrokes("home end up down left right");
        view.read_with(cx, |view, _| assert_eq!(view.navigations, 6));
        cx.simulate_keystrokes("shift-right right");
        view.read_with(cx, |view, _| assert_eq!(view.navigations, 6));
        cx.simulate_keystrokes("down secondary-c");
        view.read_with(cx, |view, _| assert_eq!(view.navigations, 7));
        assert_eq!(clipboard(cx).as_deref(), Some("whole row"));
    }

    #[gpui::test]
    fn markdown_selection_rejects_stale_pointer_activation(cx: &mut TestAppContext) {
        let (view, cx) = reply(cx, "**alpha** beta");
        view.update(cx, |view, cx| {
            view.reject_pointer = true;
            cx.notify();
        });
        cx.run_until_parked();
        let bounds = cx.debug_bounds("reply-bounds").unwrap();
        let start = bounds.origin + point(px(0.), px(8.));
        let end = start + point(px(200.), px(0.));
        cx.simulate_mouse_down(start, gpui::MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(end, gpui::MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(end, gpui::MouseButton::Left, Modifiers::none());
        cx.simulate_keystrokes("secondary-a secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some("whole row"));
        cx.update(|window, cx| assert!(view.read(cx).focus.is_focused(window)));
        cx.update(|window, cx| {
            let focus = view.read(cx).text_focus.clone();
            focus.focus(window, cx);
        });
        cx.simulate_keystrokes("secondary-shift-end secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some("alpha beta"));
    }

    #[gpui::test]
    fn markdown_selection_moves_over_visual_wrapped_rows(cx: &mut TestAppContext) {
        let source = "alpha **bold** beta ".repeat(12);
        let (_view, cx) = reply(cx, &source);
        let start = cx.debug_bounds("reply-bounds").unwrap().origin + point(px(0.), px(8.));
        cx.simulate_click(start, Modifiers::none());
        cx.simulate_keystrokes("shift-down secondary-c");
        let copied = clipboard(cx).unwrap();
        assert!("alpha bold beta ".repeat(12).starts_with(&copied));
        assert!(copied.len() > "alpha bold beta ".len());
        assert!(copied.len() < "alpha bold beta ".repeat(12).len());
        cx.simulate_keystrokes("shift-up secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some("whole row"));
        cx.simulate_keystrokes("secondary-shift-end secondary-c");
        assert_eq!(
            clipboard(cx).as_deref(),
            Some("alpha bold beta ".repeat(12).trim_end())
        );
    }

    #[gpui::test]
    fn markdown_selection_streaming_never_splits_a_changed_grapheme(cx: &mut TestAppContext) {
        let (view, cx) = reply(cx, "e");
        let start = cx.debug_bounds("reply-bounds").unwrap().origin + point(px(0.), px(8.));
        cx.simulate_click(start, Modifiers::none());
        cx.simulate_keystrokes("secondary-a secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some("e"));
        view.update(cx, |view, cx| {
            view.source.push('\u{301}');
            cx.notify();
        });
        cx.simulate_keystrokes("secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some("whole row"));
        cx.simulate_keystrokes("shift-right secondary-c");
        assert_eq!(clipboard(cx).as_deref(), Some("e\u{301}"));
    }
}
