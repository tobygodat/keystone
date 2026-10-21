//! Native Mermaid diagrams. Source remains available throughout rendering and failure.

mod fonts;
pub mod runtime;

use crate::prelude::*;
use futures_util::future::{Either, select};
use gpui::{
    AnyElement, App, ClipboardItem, Context, FocusHandle, FontStyle, FontWeight, Image,
    ImageFormat, IntoElement, Render, RenderOnce, SharedString, Task, TextStyle, Window, canvas,
    div, img, prelude::*, px,
};
use runtime::{Palette, RenderRequest, TextMeasurer};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

#[derive(IntoElement)]
pub struct Diagram {
    id: SharedString,
    source: SharedString,
    closed: bool,
    copy_focus: Option<FocusHandle>,
    toggle_focus: Option<FocusHandle>,
    managed_focus: bool,
}

impl Diagram {
    pub fn new(id: impl Into<SharedString>, source: impl Into<SharedString>, closed: bool) -> Self {
        Self {
            id: id.into(),
            source: source.into(),
            closed,
            copy_focus: None,
            toggle_focus: None,
            managed_focus: false,
        }
    }

    pub fn copy_focus(mut self, focus: &FocusHandle) -> Self {
        self.copy_focus = Some(focus.clone());
        self
    }

    pub fn toggle_focus(mut self, focus: &FocusHandle) -> Self {
        self.toggle_focus = Some(focus.clone());
        self
    }

    pub fn managed_focus(mut self) -> Self {
        self.managed_focus = true;
        self
    }
}

struct DiagramState {
    id: SharedString,
    source: SharedString,
    closed: bool,
    show_source: bool,
    copied: bool,
    width: u32,
    request: Option<RenderRequest>,
    result: Option<Result<(Arc<Image>, f32, f32), String>>,
    cancel: Arc<AtomicBool>,
    task: Option<Task<()>>,
    copy_timer: Option<Task<()>>,
    source_focus: FocusHandle,
    copy_focus: FocusHandle,
    copy_override: Option<FocusHandle>,
    toggle_override: Option<FocusHandle>,
    managed_focus: bool,
}

impl Drop for DiagramState {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl RenderOnce for Diagram {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = window.use_keyed_state(self.id.clone(), cx, |window, cx| {
            DiagramState::new(
                self.id.clone(),
                self.source.clone(),
                self.closed,
                window,
                cx,
            )
        });
        state.update(cx, |state, cx| {
            state.copy_override = self.copy_focus;
            state.toggle_override = self.toggle_focus;
            state.managed_focus = self.managed_focus;
            if state.source != self.source || state.closed != self.closed {
                state.cancel.store(true, Ordering::Relaxed);
                state.task = None;
                state.source = self.source;
                state.closed = self.closed;
                state.request = None;
                state.clear_result(window, cx);
                state.copied = false;
                state.copy_timer = None;
                cx.notify();
            }
        });
        state
    }
}

fn hex(color: gpui::Hsla) -> String {
    let color = color.to_rgb();
    format!(
        "#{:02x}{:02x}{:02x}",
        (color.r * 255.).round() as u8,
        (color.g * 255.).round() as u8,
        (color.b * 255.).round() as u8
    )
}

fn palette(cx: &App) -> Palette {
    let c = cx.theme().colors;
    Palette {
        variables: [
            ("--pele-bg", c.surface_raised),
            ("--pele-fg", c.text),
            ("--pele-muted", c.text_muted),
            ("--pele-line", c.text_muted),
            ("--pele-accent", c.accent),
            ("--pele-surface", c.element_hover),
            ("--pele-surface-alt", c.element_active),
            ("--pele-border", c.border_strong),
            ("--pele-critical", c.danger),
            ("--pele-error", c.danger),
            ("--pele-series-1", c.accent),
            ("--pele-series-2", c.success),
            ("--pele-series-3", c.warning),
            ("--pele-series-4", c.provider_claude),
            ("--pele-series-5", c.provider_codex),
            ("--pele-series-6", c.text_muted),
        ]
        .into_iter()
        .map(|(name, color)| (name.to_owned(), hex(c.surface_raised.blend(color))))
        .collect(),
    }
}

impl DiagramState {
    fn new(
        id: SharedString,
        source: SharedString,
        closed: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.on_release_in(window, |this, window, cx| this.clear_result(window, cx))
            .detach();
        Self {
            id,
            source,
            closed,
            show_source: false,
            copied: false,
            width: 0,
            request: None,
            result: None,
            cancel: Arc::new(AtomicBool::new(false)),
            task: None,
            copy_timer: None,
            source_focus: cx.focus_handle().tab_stop(true),
            copy_focus: cx.focus_handle().tab_stop(true),
            copy_override: None,
            toggle_override: None,
            managed_focus: false,
        }
    }

    fn clear_result(&mut self, window: &mut Window, cx: &mut App) {
        if let Some(Ok((image, _, _))) = self.result.take() {
            if let Some(decoded) = image.clone().get_render_image(window, cx) {
                cx.drop_image(decoded, Some(window));
            }
            image.remove_asset(cx);
        }
    }

    fn render_diagram(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.closed || self.width == 0 || self.show_source {
            return;
        }
        let font = cx.theme().fonts.ui.clone();
        let mono = cx.theme().fonts.mono.clone();
        let mut request = RenderRequest {
            source: self.source.to_string(),
            width: self.width,
            palette: palette(cx),
            font_family: font.to_string(),
            font_family_mono: mono.to_string(),
        };
        if self.request.as_ref() == Some(&request) {
            return;
        }
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.request = Some(request.clone());
        self.clear_result(window, cx);
        let cancel = self.cancel.clone();
        let timeout_cancel = cancel.clone();
        let text_system = cx.text_system().clone();
        let work = cx.background_executor().spawn(async move {
            let resolved = fonts::resolve(&request.font_family, &request.font_family_mono)?;
            let font: SharedString = resolved.ui.into();
            let mono: SharedString = resolved.mono.into();
            request.font_family = font.to_string();
            request.font_family_mono = mono.to_string();
            let mut hash = DefaultHasher::new();
            (&font, &mono).hash(&mut hash);
            let metrics_key = hash.finish();
            let text_system = gpui::WindowTextSystem::new(text_system);
            let measurer = TextMeasurer {
                cache_key: metrics_key,
                measure: Arc::new(move |text, size, style| {
                    text.split('\n')
                        .map(|line| {
                            let line: SharedString = line.replace('\r', "").into();
                            let text_style = TextStyle {
                                font_family: if style & 4 != 0 {
                                    mono.clone()
                                } else {
                                    font.clone()
                                },
                                font_weight: if style & 1 != 0 {
                                    FontWeight::BOLD
                                } else {
                                    FontWeight::NORMAL
                                },
                                font_style: if style & 2 != 0 {
                                    FontStyle::Italic
                                } else {
                                    FontStyle::Normal
                                },
                                ..Default::default()
                            };
                            f32::from(
                                text_system
                                    .shape_line(
                                        line.clone(),
                                        px(size),
                                        &[text_style.to_run(line.len())],
                                        None,
                                    )
                                    .width,
                            )
                        })
                        .fold(0., f32::max)
                }),
            };
            runtime::submit_async(request, cancel, measurer).map_err(|error| error.to_string())
        });
        let request = self.request.clone();
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = match work.await {
                Ok(ticket) => {
                    let timeout = Box::pin(cx.background_executor().timer(Duration::from_secs(4)));
                    match select(ticket, timeout).await {
                        Either::Left((result, _)) => result.map_err(|error| error.to_string()),
                        Either::Right(_) => {
                            timeout_cancel.store(true, Ordering::Relaxed);
                            Err(runtime::DiagramError::request_timeout().to_string())
                        }
                    }
                }
                Err(error) => Err(error),
            }
            .map(|rendered| {
                (
                    Arc::new(Image::from_bytes(ImageFormat::Svg, rendered.svg.to_vec())),
                    rendered.width,
                    rendered.height,
                )
            });
            let _ = this.update_in(cx, |this, window, cx| {
                if this.request == request {
                    this.clear_result(window, cx);
                    this.result = Some(result);
                    cx.notify();
                }
            });
        }));
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(self.source.to_string()));
        self.copied = true;
        cx.notify();
        self.copy_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1500))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.copied = false;
                cx.notify();
            });
        }));
    }

    fn source_element(&self, cx: &App) -> AnyElement {
        div()
            .id("source")
            .automation_id(format!("{}-source", self.id))
            .w_full()
            .overflow_x_scroll()
            .child(source_preview(
                &self.source,
                cx.theme().fonts.mono.clone(),
                cx.theme().colors.code_text,
            ))
            .into_any_element()
    }
}

fn source_preview(source: &str, mono: SharedString, color: gpui::Hsla) -> gpui::Div {
    let mut end = source.len().min(16_384);
    while !source.is_char_boundary(end) {
        end -= 1;
    }
    let mut preview = source[..end].to_owned();
    if end < source.len() {
        preview.push_str("\n… Preview limited to 16 KiB. Copy preserves the complete source.");
    }
    div()
        .p_3()
        .whitespace_nowrap()
        .font_family(mono)
        .font_features(gpui::FontFeatures::default())
        .text_size(TextSize::Ui.px())
        .line_height(px(18.))
        .text_color(color)
        .child(preview)
}

impl Render for DiagramState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_diagram(window, cx);
        let source = self.show_source || !self.closed || !matches!(self.result, Some(Ok(_)));
        let status = if !self.closed {
            "mermaid · unclosed fence"
        } else if self.task.is_some() && self.result.is_none() && !self.show_source {
            "mermaid · rendering"
        } else {
            "mermaid"
        };
        let mut body = v_flex().min_w_0().w_full();
        if let Some(Err(error)) = &self.result {
            body = body.child(
                div()
                    .p_3()
                    .text_color(cx.theme().colors.danger)
                    .automation_id(format!("{}-error", self.id))
                    .child(error.clone()),
            );
        }
        body = if source {
            body.child(self.source_element(cx))
        } else if let Some(Ok((image, width, height))) = &self.result {
            let display_width = width.min(self.width as f32);
            let height = height * display_width / width;
            let original = self.source.clone();
            let mono = cx.theme().fonts.mono.clone();
            let code_color = cx.theme().colors.code_text;
            body.child(
                div()
                    .p_3()
                    .flex()
                    .justify_center()
                    .child(
                        img(image.clone())
                            .w(px(display_width))
                            .h(px(height))
                            .with_fallback(move || {
                                div()
                                    .child("Diagram image unavailable. Source:")
                                    .child(source_preview(&original, mono.clone(), code_color))
                                    .into_any_element()
                            }),
                    )
                    .automation_id(format!("{}-image", self.id)),
            )
        } else {
            body
        };
        let weak = cx.entity().downgrade();
        v_flex()
            .id(self.id.clone())
            .min_w_0()
            .w_full()
            .surface(Elevation::Raised, cx)
            .overflow_hidden()
            .child(
                h_flex()
                    .pl_3()
                    .pr_1()
                    .py_0p5()
                    .gap_2()
                    .hairline_b(cx)
                    .child(
                        div()
                            .flex_1()
                            .child(Label::new(status).size(TextSize::Ui).color(Color::Muted)),
                    )
                    .when(self.closed, |d| {
                        d.child(
                            Button::new(
                                "toggle",
                                if self.show_source {
                                    "Diagram"
                                } else {
                                    "Source"
                                },
                            )
                            .reserve_focus_ring()
                            .when_some(
                                self.toggle_override.as_ref().or_else(|| {
                                    (!self.managed_focus).then_some(&self.source_focus)
                                }),
                                |button, focus| button.track_focus(focus).reveal_on_focus(),
                            )
                            .aria_label("Toggle Mermaid source")
                            .debug_selector(|| format!("{}-toggle", self.id))
                            .on_click(cx.listener(
                                |this, _, window, cx| {
                                    this.show_source = !this.show_source;
                                    if this.show_source && !matches!(this.result, Some(Ok(_))) {
                                        this.cancel.store(true, Ordering::Relaxed);
                                        this.task = None;
                                        this.request = None;
                                        this.clear_result(window, cx);
                                    }
                                    cx.notify();
                                },
                            )),
                        )
                    })
                    .child(
                        Button::new("copy", if self.copied { "Copied" } else { "Copy" })
                            .reserve_focus_ring()
                            .when_some(
                                self.copy_override
                                    .as_ref()
                                    .or_else(|| (!self.managed_focus).then_some(&self.copy_focus)),
                                |button, focus| button.track_focus(focus).reveal_on_focus(),
                            )
                            .aria_label("Copy Mermaid source")
                            .debug_selector(|| format!("{}-copy", self.id))
                            .on_click(cx.listener(|this, _, _, cx| this.copy(cx))),
                    ),
            )
            .child(
                div().w_full().min_w_0().relative().child(body).child(
                    canvas(
                        move |bounds, _, cx| {
                            let width = (f32::from(bounds.size.width) - 24.).floor().max(1.) as u32;
                            if weak
                                .read_with(cx, |this, _| this.width != width)
                                .unwrap_or(false)
                            {
                                let weak = weak.clone();
                                cx.defer(move |cx| {
                                    let _ = weak.update(cx, |this, cx| {
                                        this.width = width;
                                        cx.notify();
                                    });
                                });
                            }
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .inset_0(),
                ),
            )
    }
}

#[cfg(all(test, feature = "ui-tests"))]
mod tests {
    use super::*;
    use gpui::{Entity, TestAppContext};

    struct AtlasHarness {
        diagram: Option<Entity<DiagramState>>,
        width: f32,
    }

    impl Render for AtlasHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w(px(self.width)).children(self.diagram.clone())
        }
    }

    #[gpui::test]
    fn replacement_and_release_evict_decoded_images_and_gpu_atlas_tiles(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        for change in ["theme", "width", "release"] {
            cx.update(crate::init);
            let image = Arc::new(Image::from_bytes(ImageFormat::Svg,
                br#"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="40"><rect width="200" height="40"/></svg>"#.to_vec()));
            let (view, cx) = cx.add_window_view(|window, cx| {
                let diagram = cx.new(|cx| {
                    let mut state =
                        DiagramState::new("atlas-test".into(), "invalid".into(), true, window, cx);
                    state.width = 198;
                    state.request = Some(RenderRequest {
                        source: state.source.to_string(),
                        width: 198,
                        palette: palette(cx),
                        font_family: cx.theme().fonts.ui.to_string(),
                        font_family_mono: cx.theme().fonts.mono.to_string(),
                    });
                    state.result = Some(Ok((image.clone(), 200., 40.)));
                    state
                });
                AtlasHarness {
                    diagram: Some(diagram),
                    width: 224.,
                }
            });
            let deadline = std::time::Instant::now() + Duration::from_secs(4);
            let decoded = loop {
                cx.run_until_parked();
                cx.update(|window, cx| window.draw(cx).clear(cx));
                if let Some(decoded) =
                    cx.update(|window, cx| image.clone().get_render_image(window, cx))
                {
                    if cx.update(|window, _| window.has_image_atlas_entry(&decoded)) {
                        break decoded;
                    }
                }
                assert!(std::time::Instant::now() < deadline, "SVG must paint");
                std::thread::sleep(Duration::from_millis(5));
            };
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.update(|window, _| {
                assert!(
                    window.has_image_atlas_entry(&decoded),
                    "{change}: image painted"
                )
            });
            match change {
                "theme" => cx.update(|_, cx| crate::set_theme(crate::Theme::light(), cx)),
                "width" => view.update(cx, |view, cx| {
                    view.width = 320.;
                    cx.notify();
                }),
                "release" => view.update(cx, |view, cx| {
                    view.diagram = None;
                    cx.notify();
                }),
                _ => unreachable!(),
            }
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.run_until_parked();
            cx.update(|window, cx| {
                assert!(
                    !window.has_image_atlas_entry(&decoded),
                    "{change}: old GPU tile must be released"
                );
                assert!(
                    !image.is_asset_cached(cx),
                    "{change}: old decode must be released"
                );
            });
        }
    }
}
