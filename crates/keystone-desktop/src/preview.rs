//! Images and videos in a conversation. A thumbnail is read, decoded and downscaled on
//! a background thread; clicking it opens [`Overlay`], which shows the image at fit or
//! at 1:1. A video is a card that opens in the system player. Nothing is fetched from
//! the network: replies' remote images stay links in the Markdown.

use futures::channel::oneshot;
use gpui::{
    AnyElement, App, Asset, ClipboardItem, Context, DismissEvent, ElementId, EventEmitter,
    FocusHandle, Focusable, IntoElement, KeyDownEvent, RenderImage, RenderOnce, SharedString,
    Window, anchored, deferred, div, img, point, prelude::*, px, size,
};
use image::metadata::Orientation;
use image::{DynamicImage, Frame, ImageDecoder, ImageReader, Limits};
use keystone_agent::ItemStatus;
use keystone_ui::markdown::{ImageRef, ImageRenderer};
use keystone_ui::prelude::*;
use std::cell::Cell;
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

/// The longest side a thumbnail is decoded at: twice its largest drawn size, for HiDPI.
const THUMBNAIL_PIXELS: u32 = 720;
/// The longest side the overlay decodes at. Larger images are downscaled to it.
const FULL_PIXELS: u32 = 4096;
/// The longest side of an attachment chip's image: twice its 28 pixels, rounded up.
const CHIP_PIXELS: u32 = 64;
/// Files larger than this are not decoded.
const MAX_BYTES: u64 = 64 * 1024 * 1024;
const THUMBNAIL_WIDTH: f32 = 360.;
const THUMBNAIL_HEIGHT: f32 = 200.;
/// How many decoded thumbnails and chips stay cached; older ones are decoded again
/// when they next show.
const KEEP: usize = 48;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Image,
    Video,
}

/// What a path shows as, from its extension.
pub(crate) fn kind(path: &Path) -> Option<Kind> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => Some(Kind::Image),
        "mp4" | "webm" | "mov" => Some(Kind::Video),
        _ => None,
    }
}

/// A file to show: `path`, which must be inside `within` when it is set. `pixels` is
/// the longest side to decode at, or 0 to only check the file. `row` is the transcript
/// row showing it, so a file an agent rewrites is read again for the row that says so,
/// and `done` whether that row has finished, so a file read while the agent was still
/// writing it is read again once it has.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct Request {
    path: PathBuf,
    within: Option<PathBuf>,
    pixels: u32,
    row: usize,
    done: bool,
}

/// A checked file: its canonical path and, when decoded, its image and the image's
/// size in the file, before any downscaling.
#[derive(Clone)]
struct Media {
    path: PathBuf,
    image: Option<(Arc<RenderImage>, gpui::Size<u32>)>,
}

enum Load {}

impl Asset for Load {
    type Source = Request;
    type Output = Option<Media>;

    fn load(request: Request, _: &mut App) -> impl Future<Output = Self::Output> + Send + 'static {
        async move { load(&request) }
    }
}

/// The file `request` names, decoded when it asks for pixels, or `None` when it is
/// missing, outside its folder, too large, or not an image.
fn load(request: &Request) -> Option<Media> {
    let path = request.path.canonicalize().ok()?;
    if let Some(root) = &request.within
        && !path.starts_with(root.canonicalize().ok()?)
    {
        return None;
    }
    let metadata = std::fs::metadata(&path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    if request.pixels == 0 {
        return Some(Media { path, image: None });
    }
    if metadata.len() > MAX_BYTES {
        return None;
    }
    let mut decoder = ImageReader::open(&path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .into_decoder()
        .ok()?;
    Limits::default().reserve(decoder.total_bytes()).ok()?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder).ok()?;
    image.apply_orientation(orientation);
    let max = request.pixels;
    let source = size(image.width(), image.height());
    let image = if image.width() > max || image.height() > max {
        image.thumbnail(max, max)
    } else {
        image
    };
    let mut buffer = image.into_rgba8();
    for pixel in buffer.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Some(Media {
        path,
        image: Some((Arc::new(RenderImage::new(vec![Frame::new(buffer)])), source)),
    })
}

/// The thumbnail and chip requests shown recently, oldest first, and those shown in
/// the frame being drawn.
#[derive(Default)]
struct Recent {
    order: VecDeque<Request>,
    drawing: HashSet<Request>,
}

impl gpui::Global for Recent {}

/// `request`'s file once it has loaded. The [`KEEP`] most recently shown stay in GPUI's
/// asset cache, with any shown in the current frame; after the frame, older ones are
/// dropped. While a finished row's request loads, its unfinished one stands in.
fn use_media(request: &Request, window: &mut Window, cx: &mut App) -> Option<Option<Media>> {
    if cx.default_global::<Recent>().drawing.is_empty() {
        cx.defer(|cx| {
            let recent = cx.default_global::<Recent>();
            let drawn = std::mem::take(&mut recent.drawing);
            for stale in sweep(&mut recent.order, &drawn) {
                forget(&stale, None, cx);
            }
        });
    }
    let recent = cx.default_global::<Recent>();
    remember(&mut recent.order, request);
    recent.drawing.insert(request.clone());
    let early = Request {
        done: false,
        ..request.clone()
    };
    let early = (request.done && recent.order.contains(&early)).then(|| {
        recent.drawing.insert(early.clone());
        early
    });
    window
        .use_asset::<Load>(request, cx)
        .or_else(|| cx.fetch_asset::<Load>(&early?))
}

/// Drops `request`'s cached file and the GPU texture of its image.
fn forget(request: &Request, window: Option<&mut Window>, cx: &mut App) {
    if let Some(Some(Media {
        image: Some((image, _)),
        ..
    })) = cx.fetch_asset::<Load>(request)
    {
        cx.drop_image(image, window);
    }
    cx.remove_asset::<Load>(request);
}

/// Moves `request` to the newest end of `order`.
fn remember(order: &mut VecDeque<Request>, request: &Request) {
    if order.back() != Some(request) {
        order.retain(|shown| shown != request);
        order.push_back(request.clone());
    }
}

/// Removes and returns the oldest requests past [`KEEP`], except those in `drawn`.
fn sweep(order: &mut VecDeque<Request>, drawn: &HashSet<Request>) -> Vec<Request> {
    let mut excess = order.len().saturating_sub(KEEP);
    let mut stale = Vec::new();
    order.retain(|request| {
        let keep = excess == 0 || drawn.contains(request);
        if !keep {
            excess -= 1;
            stale.push(request.clone());
        }
        keep
    });
    stale
}

/// An image of `pixels` in window pixels, shrunk to fit `bounds` when it is larger.
fn fitted(
    pixels: gpui::Size<u32>,
    bounds: gpui::Size<f32>,
    window: &Window,
) -> gpui::Size<gpui::Pixels> {
    let scale = window.scale_factor();
    let (width, height) = (pixels.width as f32 / scale, pixels.height as f32 / scale);
    let shrink = (bounds.width / width).min(bounds.height / height).min(1.);
    size(px((width * shrink).floor()), px((height * shrink).floor()))
}

pub(crate) type OpenPreview = Rc<dyn Fn(PathBuf, &mut Window, &mut App)>;

/// How a conversation's rows show media: the session's folder, which a reply's images
/// must be inside, and what clicking a thumbnail does.
#[derive(Clone)]
pub(crate) struct Previews {
    pub(crate) root: Option<PathBuf>,
    pub(crate) open: OpenPreview,
}

impl Previews {
    /// Draws a reply's local images, numbering them in order for row `row`.
    pub(crate) fn markdown(&self, row: usize, done: bool) -> ImageRenderer {
        let previews = self.clone();
        let count = Rc::new(Cell::new(0));
        Rc::new(move |image: &ImageRef, _: &mut Window, _: &mut App| {
            let fallback = || {
                Label::new(if image.alt.trim().is_empty() {
                    image.url.clone()
                } else {
                    image.alt.clone()
                })
                .color(Color::Muted)
                .into_any_element()
            };
            let Some(root) = &previews.root else {
                return fallback();
            };
            let path = root.join(decode_percent(&image.url));
            let n = count.get();
            count.set(n + 1);
            previews
                .media(row, done, n, path, Some(root.clone()), Some(fallback()))
                .unwrap_or_else(fallback)
        })
    }

    /// Thumbnails and cards for the images and videos a file change touched. A file
    /// no longer there shows nothing, since the row already names it. Until the change
    /// has completed, the agent has not written the file, so it must be inside the
    /// session's folder like a reply's.
    pub(crate) fn changed(
        &self,
        row: usize,
        status: ItemStatus,
        paths: &[String],
    ) -> Option<AnyElement> {
        let done = status != ItemStatus::InProgress;
        let within = match status {
            ItemStatus::Completed => None,
            _ => Some(self.root.clone()?),
        };
        let media: Vec<AnyElement> = paths
            .iter()
            .enumerate()
            .filter_map(|(n, path)| {
                let path = match &self.root {
                    Some(root) => root.join(path),
                    None => Some(PathBuf::from(path)).filter(|path| path.is_absolute())?,
                };
                self.media(row, done, n, path, within.clone(), None)
            })
            .collect();
        (!media.is_empty()).then(|| {
            h_flex()
                .flex_wrap()
                .gap_2()
                .children(media)
                .into_any_element()
        })
    }

    /// The thumbnail or video card for `path`, which must be inside `within` when it is
    /// set. A reply's media passes what to draw when it is not shown.
    fn media(
        &self,
        row: usize,
        done: bool,
        n: usize,
        path: PathBuf,
        within: Option<PathBuf>,
        fallback: Option<AnyElement>,
    ) -> Option<AnyElement> {
        let kind = kind(&path)?;
        let request = |pixels| Request {
            path,
            within,
            pixels,
            row,
            done,
        };
        Some(match kind {
            Kind::Image => Thumbnail {
                id: format!("thumbnail-{row}-{n}").into(),
                request: request(THUMBNAIL_PIXELS),
                fallback,
                open: self.open.clone(),
            }
            .into_any_element(),
            Kind::Video => VideoCard {
                id: format!("video-{row}-{n}").into(),
                named: fallback.is_some(),
                request: request(0),
                fallback,
            }
            .into_any_element(),
        })
    }
}

/// `%XX` escapes in a Markdown destination, as in `my%20shot.png`.
fn decode_percent(url: &str) -> PathBuf {
    let bytes = url.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(&a), Some(&b)) = (bytes.get(i + 1), bytes.get(i + 2))
            && let (Some(a), Some(b)) = (hex(a), hex(b))
        {
            out.push((a * 16 + b) as u8);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    PathBuf::from(String::from_utf8_lossy(&out).into_owned())
}

/// An image file shrunk to fit a 360 by 200 tile, which it holds while it decodes so
/// the row does not move. A file that cannot be shown draws `fallback`.
#[derive(IntoElement)]
pub(crate) struct Thumbnail {
    id: SharedString,
    request: Request,
    fallback: Option<AnyElement>,
    open: OpenPreview,
}

impl Thumbnail {
    /// A thumbnail in transcript row `row` of the image at `path`, which is trusted,
    /// such as an attachment's copy.
    pub(crate) fn new(
        id: impl Into<SharedString>,
        path: PathBuf,
        row: usize,
        open: OpenPreview,
    ) -> Self {
        Self {
            id: id.into(),
            request: Request {
                path,
                within: None,
                pixels: THUMBNAIL_PIXELS,
                row,
                done: true,
            },
            fallback: None,
            open,
        }
    }

    pub(crate) fn fallback(mut self, fallback: AnyElement) -> Self {
        self.fallback = Some(fallback);
        self
    }
}

impl RenderOnce for Thumbnail {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let colors = cx.theme().colors;
        let tile = || {
            div()
                .flex_none()
                .w(px(THUMBNAIL_WIDTH + 2.))
                .h(px(THUMBNAIL_HEIGHT + 2.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(radius::CONTROL)
                .overflow_hidden()
                .border_1()
                .border_color(colors.border)
                .bg(colors.surface_raised)
        };
        match use_media(&self.request, window, cx) {
            None => tile().into_any_element(),
            Some(Some(Media {
                path,
                image: Some((image, pixels)),
            })) => {
                let shown = fitted(pixels, size(THUMBNAIL_WIDTH, THUMBNAIL_HEIGHT), window);
                let open = self.open;
                tile()
                    .id(ElementId::Name(self.id.clone()))
                    .automation_id(self.id.to_string())
                    .cursor_pointer()
                    .child(img(image).w(shown.width).h(shown.height))
                    .on_click(move |_, window, cx| open(path.clone(), window, cx))
                    .into_any_element()
            }
            Some(_) => self.fallback.unwrap_or_else(|| div().into_any_element()),
        }
    }
}

/// A video file's Open in player, which waits for the file to be found, after its name
/// when `named`. A file that is not found draws `fallback`.
#[derive(IntoElement)]
struct VideoCard {
    id: SharedString,
    named: bool,
    request: Request,
    fallback: Option<AnyElement>,
}

impl RenderOnce for VideoCard {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let media = match use_media(&self.request, window, cx) {
            Some(None) => return self.fallback.unwrap_or_else(|| div().into_any_element()),
            media => media.flatten(),
        };
        let name = self
            .named
            .then(|| self.request.path.file_name())
            .flatten()
            .map(|name| name.to_string_lossy().into_owned());
        let open = format!("{}-open", self.id);
        h_flex()
            .automation_id(self.id.to_string())
            .flex_none()
            .max_w(px(THUMBNAIL_WIDTH))
            .gap_2()
            .children(name.map(|name| div().min_w_0().child(Label::new(name).truncate())))
            .child(match media {
                Some(media) => {
                    OpenButton::new(open, "Open in player", ButtonStyle::Subtle, media.path)
                        .into_any_element()
                }
                None => Button::new(ElementId::Name(open.into()), "Open in player")
                    .style(ButtonStyle::Subtle)
                    .disabled(true)
                    .into_any_element(),
            })
            .into_any_element()
    }
}

/// Opens a file with the system's default app, and says so beside it when that fails.
#[derive(IntoElement)]
struct OpenButton {
    id: SharedString,
    label: &'static str,
    style: ButtonStyle,
    path: PathBuf,
}

impl OpenButton {
    fn new(
        id: impl Into<SharedString>,
        label: &'static str,
        style: ButtonStyle,
        path: PathBuf,
    ) -> Self {
        Self {
            id: id.into(),
            label,
            style,
            path,
        }
    }
}

impl RenderOnce for OpenButton {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let error = window.use_keyed_state(
            ElementId::Name(format!("{}-error", self.id).into()),
            cx,
            |_, _| false,
        );
        let failed = *error.read(cx);
        let id = self.id.clone();
        let path = self.path;
        h_flex()
            .gap_2()
            .when(failed, |row| {
                row.child(
                    div().automation_id(format!("{}-error", self.id)).child(
                        Label::new("Couldn't open")
                            .size(TextSize::Ui)
                            .color(Color::Danger),
                    ),
                )
            })
            .child(
                Button::new(ElementId::Name(self.id), self.label)
                    .style(self.style)
                    .debug_selector(move || id.to_string())
                    .on_click(move |_, _, cx| {
                        let (sender, receiver) = oneshot::channel();
                        let path = path.clone();
                        std::thread::spawn(move || {
                            let _ = sender.send(!crate::files::open_with_system(&path));
                        });
                        let error = error.clone();
                        cx.spawn(async move |cx| {
                            let Ok(failed) = receiver.await else {
                                return;
                            };
                            error.update(cx, |error, cx| {
                                *error = failed;
                                cx.notify();
                            });
                        })
                        .detach();
                    }),
            )
    }
}

/// The small square image of an attachment chip, downscaled like a thumbnail. A copy
/// that cannot be read shows a file icon.
#[derive(IntoElement)]
pub(crate) struct ChipImage {
    request: Request,
}

impl ChipImage {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            request: Request {
                path,
                within: None,
                pixels: CHIP_PIXELS,
                row: 0,
                done: true,
            },
        }
    }
}

impl RenderOnce for ChipImage {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let image = match use_media(&self.request, window, cx) {
            Some(Some(Media {
                image: Some((image, _)),
                ..
            })) => img(image)
                .size_full()
                .object_fit(gpui::ObjectFit::Cover)
                .into_any_element(),
            Some(_) => Icon::new(IconName::File)
                .size(px(16.))
                .color(Color::Muted)
                .into_any_element(),
            None => div().into_any_element(),
        };
        div()
            .size(px(28.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.))
            .overflow_hidden()
            .child(image)
    }
}

/// An image over the whole window, at fit or at 1:1, with Open externally and Copy
/// path. Escape, the close button or a press outside the image dismisses it.
pub(crate) struct Overlay {
    path: PathBuf,
    actual: bool,
    copied: bool,
    focus: FocusHandle,
}

impl EventEmitter<DismissEvent> for Overlay {}

impl Focusable for Overlay {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Overlay {
    pub(crate) fn new(path: PathBuf, cx: &mut Context<Self>) -> Self {
        Self {
            path,
            actual: false,
            copied: false,
            focus: cx.focus_handle(),
        }
    }

    fn request(&self) -> Request {
        Request {
            path: self.path.clone(),
            within: None,
            pixels: FULL_PIXELS,
            row: 0,
            done: true,
        }
    }

    fn set_actual(&mut self, actual: bool, cx: &mut Context<Self>) {
        self.actual = actual;
        cx.notify();
    }

    fn copy_path(&mut self, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(
            self.path.to_string_lossy().into_owned(),
        ));
        self.copied = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1500))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.copied = false;
                cx.notify();
            });
        })
        .detach();
    }
}

impl gpui::Render for Overlay {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors;
        let viewport = window.viewport_size();
        let margin = 24.;
        let toolbar = 44.;
        let bounds = size(
            (f32::from(viewport.width) - 2. * margin).max(1.),
            (f32::from(viewport.height) - 2. * margin - toolbar).max(1.),
        );
        let image = match window.use_asset::<Load>(&self.request(), cx) {
            Some(Some(Media {
                image: Some((image, pixels)),
                ..
            })) => {
                let shown = if self.actual {
                    fitted(pixels, size(f32::MAX, f32::MAX), window)
                } else {
                    fitted(pixels, bounds, window)
                };
                let picture = div()
                    .id("preview-image")
                    .automation_id("preview-image")
                    .flex_none()
                    .occlude()
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(img(image).w(shown.width).h(shown.height));
                if self.actual {
                    div()
                        .id("preview-scroll")
                        .size_full()
                        .overflow_scroll()
                        .child(
                            div()
                                .min_w_full()
                                .min_h_full()
                                .w(shown.width)
                                .h(shown.height)
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(picture),
                        )
                        .into_any_element()
                } else {
                    picture.into_any_element()
                }
            }
            Some(_) => Label::new("Couldn't read image")
                .color(Color::Muted)
                .into_any_element(),
            None => div().into_any_element(),
        };
        let actual = self.actual;
        let toolbar = h_flex()
            .id("preview-toolbar")
            .h(px(toolbar))
            .flex_none()
            .justify_end()
            .child(
                h_flex()
                    .id("preview-buttons")
                    .occlude()
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .gap_1()
                    .p(px(4.))
                    .surface(Elevation::Overlay, cx)
                    .child(
                        Button::new(
                            "preview-copy",
                            if self.copied { "Copied" } else { "Copy path" },
                        )
                        .debug_selector(|| "preview-copy".into())
                        .on_click(cx.listener(|this, _, _, cx| this.copy_path(cx))),
                    )
                    .child(OpenButton::new(
                        "preview-open",
                        "Open externally",
                        ButtonStyle::Ghost,
                        self.path.clone(),
                    ))
                    .child(
                        Button::new("preview-fit", "Fit")
                            .selected(!actual)
                            .debug_selector(|| "preview-fit".into())
                            .on_click(cx.listener(|this, _, _, cx| this.set_actual(false, cx))),
                    )
                    .child(
                        Button::new("preview-actual", "1:1")
                            .selected(actual)
                            .debug_selector(|| "preview-actual".into())
                            .on_click(cx.listener(|this, _, _, cx| this.set_actual(true, cx))),
                    )
                    .child(
                        IconButton::new("preview-close", IconName::Close)
                            .tooltip(Tooltip::with_key("Close", "Esc"))
                            .debug_selector(|| "preview-close".into())
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    ),
            );
        let scrim = v_flex()
            .id("preview-overlay")
            .automation_id("preview-overlay")
            .key_context(ConfirmDialog::KEY_CONTEXT)
            .track_focus(&self.focus)
            .w(viewport.width)
            .h(viewport.height)
            .p(px(margin))
            .occlude()
            .bg(colors.scrim)
            .on_key_down(cx.listener(|_, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    cx.emit(DismissEvent);
                    cx.stop_propagation();
                }
            }))
            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
            .child(toolbar)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(image),
            );
        deferred(
            anchored()
                .position_mode(gpui::AnchoredPositionMode::Window)
                .position(point(px(0.), px(0.)))
                .child(scrim),
        )
        .priority(2)
    }
}

/// The open overlay, and what had focus before it.
pub(crate) struct Open {
    overlay: gpui::Entity<Overlay>,
    restore: Option<FocusHandle>,
    _dismiss: gpui::Subscription,
}

impl Open {
    pub(crate) fn overlay(&self) -> gpui::Entity<Overlay> {
        self.overlay.clone()
    }
}

impl crate::Keystone {
    /// What clicking a thumbnail in this window does.
    pub(crate) fn previews(&self, session: u64, cx: &Context<Self>) -> Previews {
        let activation = self.presentation.activation();
        let entity = cx.weak_entity();
        Previews {
            root: self.session_root(session, cx),
            open: Rc::new(move |path, window, cx| {
                let _ = entity.update(cx, |this, cx| {
                    if this.presentation.matches(activation) {
                        this.open_preview(path, window, cx);
                    }
                });
            }),
        }
    }

    pub(crate) fn open_preview(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let restore = match self.preview.take() {
            Some(open) => {
                forget(&open.overlay.read(cx).request(), Some(window), cx);
                open.restore
            }
            None => window.focused(cx),
        };
        let overlay = cx.new(|cx| Overlay::new(path, cx));
        let dismiss = cx.subscribe_in(&overlay, window, |this, _, _: &DismissEvent, window, cx| {
            this.close_preview(window, cx)
        });
        let focus = overlay.read(cx).focus.clone();
        window.focus(&focus, cx);
        self.preview = Some(Open {
            overlay,
            restore,
            _dismiss: dismiss,
        });
        cx.notify();
    }

    pub(crate) fn close_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.preview.take() else {
            return;
        };
        forget(&open.overlay.read(cx).request(), Some(window), cx);
        if let Some(restore) = open.restore {
            window.focus(&restore, cx);
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(dir: &Path, name: &str, width: u32, height: u32) -> PathBuf {
        let path = dir.join(name);
        image::RgbaImage::from_pixel(width, height, image::Rgba([10, 20, 30, 255]))
            .save(&path)
            .unwrap();
        path
    }

    #[test]
    fn media_kinds_come_from_the_extension() {
        assert_eq!(kind(Path::new("a/shot.PNG")), Some(Kind::Image));
        assert_eq!(kind(Path::new("out/demo.mp4")), Some(Kind::Video));
        assert_eq!(kind(Path::new("clip.mov")), Some(Kind::Video));
        assert_eq!(kind(Path::new("notes.md")), None);
        assert_eq!(kind(Path::new("png")), None);
    }

    #[test]
    fn large_images_are_downscaled_and_small_ones_kept() {
        let dir = tempfile::tempdir().unwrap();
        let request = |path: PathBuf| Request {
            path,
            within: Some(dir.path().to_owned()),
            pixels: THUMBNAIL_PIXELS,
            row: 0,
            done: true,
        };
        let big = load(&request(png(dir.path(), "big.png", 3000, 1500))).unwrap();
        let (image, source) = big.image.unwrap();
        let size = image.size(0);
        assert_eq!((size.width.0, size.height.0), (720, 360));
        assert_eq!((source.width, source.height), (3000, 1500));
        let small = load(&request(png(dir.path(), "small.png", 40, 30))).unwrap();
        let size = small.image.unwrap().0.size(0);
        assert_eq!((size.width.0, size.height.0), (40, 30));
    }

    #[test]
    fn decoded_pixels_are_bgra() {
        let dir = tempfile::tempdir().unwrap();
        let media = load(&Request {
            path: png(dir.path(), "a.png", 2, 2),
            within: None,
            pixels: THUMBNAIL_PIXELS,
            row: 0,
            done: true,
        })
        .unwrap();
        assert_eq!(
            &media.image.unwrap().0.as_bytes(0).unwrap()[..4],
            &[30, 20, 10, 255]
        );
    }

    #[test]
    fn a_reply_image_must_be_inside_the_session_folder() {
        let outside = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let escaped = png(outside.path(), "secret.png", 4, 4);
        std::os::unix::fs::symlink(&escaped, root.path().join("link.png")).unwrap();
        for path in [
            escaped.clone(),
            root.path()
                .join("../")
                .join(outside.path().file_name().unwrap())
                .join("secret.png"),
            root.path().join("link.png"),
        ] {
            let result = load(&Request {
                path: path.clone(),
                within: Some(root.path().to_owned()),
                pixels: THUMBNAIL_PIXELS,
                row: 0,
                done: true,
            });
            assert!(result.is_none(), "{path:?}");
        }
        let inside = png(root.path(), "shot.png", 4, 4);
        let media = load(&Request {
            path: inside,
            within: Some(root.path().to_owned()),
            pixels: 0,
            row: 0,
            done: true,
        })
        .unwrap();
        assert!(media.image.is_none());
    }

    #[test]
    fn missing_and_undecodable_files_fail() {
        let dir = tempfile::tempdir().unwrap();
        let broken = dir.path().join("broken.png");
        std::fs::write(&broken, b"not a png").unwrap();
        for path in [dir.path().join("gone.png"), broken, dir.path().to_owned()] {
            let request = Request {
                path,
                within: None,
                pixels: THUMBNAIL_PIXELS,
                row: 0,
                done: true,
            };
            assert!(load(&request).is_none(), "{request:?}");
        }
    }

    #[test]
    fn only_the_most_recently_shown_stay_decoded() {
        let request = |n: usize| Request {
            path: PathBuf::from(format!("/{n}.png")),
            within: None,
            pixels: THUMBNAIL_PIXELS,
            row: n,
            done: true,
        };
        let mut order = VecDeque::new();
        for n in 0..=KEEP {
            remember(&mut order, &request(n));
        }
        remember(&mut order, &request(0));
        assert_eq!(sweep(&mut order, &HashSet::new()), [request(1)]);
        assert_eq!(order.len(), KEEP);
        assert_eq!(order.back(), Some(&request(0)));

        let drawn: HashSet<Request> = (0..KEEP + 10).map(request).collect();
        for n in 0..KEEP + 10 {
            remember(&mut order, &request(n));
        }
        assert!(
            sweep(&mut order, &drawn).is_empty(),
            "a frame's own are kept"
        );
        assert_eq!(order.len(), KEEP + 10);
        assert_eq!(sweep(&mut order, &HashSet::new()).len(), 10);
    }

    #[test]
    fn percent_escapes_decode() {
        assert_eq!(
            decode_percent("my%20shot.png"),
            PathBuf::from("my shot.png")
        );
        assert_eq!(decode_percent("100%.png"), PathBuf::from("100%.png"));
        assert_eq!(decode_percent("%zz.png"), PathBuf::from("%zz.png"));
    }
}

#[cfg(all(test, feature = "ui-tests"))]
#[path = "preview_tests.rs"]
mod ui_tests;
