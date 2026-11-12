//! A captured frame decoded off the UI thread into the panel's image and the model's
//! copy.

use gpui::RenderImage;
use image::{DynamicImage, ImageFormat, imageops::FilterType};
use keystone_agent::device::Frame;
use keystone_agent::mcp::Image;
use std::io::Cursor;
use std::sync::Arc;
use std::time::Instant;

/// The longest edge of the panel's copy of a frame.
const PANEL_EDGE: u32 = 1200;
/// The model's copy of a frame fits this edge and this many pixels.
const MODEL_EDGE: f64 = 1568.;
const MODEL_PIXELS: f64 = 1_150_000.;
const MODEL_BYTES: usize = 1024 * 1024;

/// The panel's copy of a frame, with the frame's full size and when it was taken.
#[derive(Clone)]
pub(crate) struct Picture {
    pub(crate) image: Arc<RenderImage>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) at: Instant,
}

/// The model's copy of a screenshot and its size, or why it could not be made.
pub(crate) type ModelShot = Result<(Image, (u32, u32)), String>;

fn decode(frame: &Frame) -> Option<DynamicImage> {
    image::load_from_memory_with_format(&frame.png, ImageFormat::Png).ok()
}

/// The panel's copy of `frame`, or `None` when it can't be decoded.
pub(crate) fn for_panel(frame: &Frame) -> Option<Picture> {
    decode(frame).map(|decoded| picture(&decoded, frame, Instant::now()))
}

/// The panel's copy of `frame` and the model's.
pub(crate) fn for_both(frame: &Frame) -> (Option<Picture>, ModelShot) {
    match decode(frame) {
        Some(decoded) => (
            Some(picture(&decoded, frame, Instant::now())),
            for_model(&decoded),
        ),
        None => (None, Err("The screenshot could not be decoded".into())),
    }
}

fn picture(decoded: &DynamicImage, frame: &Frame, at: Instant) -> Picture {
    let small = if decoded.width().max(decoded.height()) > PANEL_EDGE {
        decoded.thumbnail(PANEL_EDGE, PANEL_EDGE)
    } else {
        decoded.clone()
    };
    let mut buffer = small.into_rgba8();
    for pixel in buffer.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Picture {
        image: Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])),
        width: frame.width,
        height: frame.height,
        at,
    }
}

/// Fits the frame to the largest image a model reads at full detail: a 1568 px long
/// edge and 1.15 megapixels. PNG, or JPEG when the PNG is over 1 MiB.
pub(crate) fn for_model(decoded: &DynamicImage) -> ModelShot {
    let (width, height) = (f64::from(decoded.width()), f64::from(decoded.height()));
    let scale = (MODEL_EDGE / width.max(height))
        .min((MODEL_PIXELS / (width * height)).sqrt())
        .min(1.);
    let scaled = if scale < 1. {
        decoded.resize_exact(
            ((width * scale).round() as u32).max(1),
            ((height * scale).round() as u32).max(1),
            FilterType::Triangle,
        )
    } else {
        decoded.clone()
    };
    let size = (scaled.width(), scaled.height());
    let mut png = Vec::new();
    scaled
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .map_err(|e| format!("The screenshot could not be encoded: {e}"))?;
    if png.len() <= MODEL_BYTES {
        return Ok((
            Image {
                mime: "image/png",
                data: png,
            },
            size,
        ));
    }
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 85)
        .encode_image(&DynamicImage::ImageRgb8(scaled.to_rgb8()))
        .map_err(|e| format!("The screenshot could not be encoded: {e}"))?;
    Ok((
        Image {
            mime: "image/jpeg",
            data: jpeg,
        },
        size,
    ))
}
