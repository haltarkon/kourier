//! Decoding with orientation and limits; thumbnails and blurhash (§5.8).

use std::io::Cursor;

use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, RgbImage};

use crate::{Encoded, MediaError};

/// Largest width or height [`decode`] accepts.
pub const MAX_DIMENSION: u32 = 16_384;
/// Largest decoder allocation [`decode`] allows (bytes).
pub const MAX_ALLOC: u64 = 256 * 1024 * 1024;
/// Longest edge of a thumbnail from [`derive`] (px).
pub const THUMBNAIL_EDGE: u32 = 400;

/// JPEG quality of thumbnails.
const THUMBNAIL_QUALITY: u8 = 80;
/// Longest edge of the image a blurhash is computed from: the hash only keeps a few components.
const BLURHASH_SOURCE_EDGE: u32 = 64;
/// Largest placeholder [`blurhash_rgba`] renders; the UI scales it up.
const MAX_PLACEHOLDER_EDGE: u32 = 256;

/// What [`probe`] learns without decoding pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageInfo {
    /// Display width (orientation applied).
    pub width: u32,
    /// Display height (orientation applied).
    pub height: u32,
    /// MIME type of the container.
    pub mime: &'static str,
}

/// A decoded image, orientation applied.
#[derive(Clone)]
pub struct Decoded {
    image: DynamicImage,
    format: ImageFormat,
}

impl std::fmt::Debug for Decoded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decoded")
            .field("width", &self.width())
            .field("height", &self.height())
            .field("format", &self.format)
            .finish()
    }
}

/// What the core stores for an image attachment (§5.8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Derived {
    /// Display width.
    pub width: u32,
    /// Display height.
    pub height: u32,
    /// Blurhash placeholder.
    pub blurhash: String,
    /// Thumbnail, longest edge ≤ [`THUMBNAIL_EDGE`] (PNG when the image has alpha, else JPEG).
    pub thumbnail: Encoded,
}

fn reader(bytes: &[u8]) -> Result<ImageReader<Cursor<&[u8]>>, MediaError> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| MediaError::Decode(e.to_string()))?;
    if reader.format().is_none() {
        return Err(MediaError::Unsupported);
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);
    Ok(reader)
}

fn swaps_axes(o: Orientation) -> bool {
    matches!(
        o,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    )
}

/// The image's display size and type, without decoding pixels.
pub fn probe(bytes: &[u8]) -> Result<ImageInfo, MediaError> {
    let reader = reader(bytes)?;
    let format = reader.format().ok_or(MediaError::Unsupported)?;
    let mut decoder = reader.into_decoder().map_err(MediaError::from_image)?;
    let (w, h) = decoder.dimensions();
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let (width, height) = if swaps_axes(orientation) {
        (h, w)
    } else {
        (w, h)
    };
    Ok(ImageInfo {
        width,
        height,
        mime: format.to_mime_type(),
    })
}

/// Decodes an image (the first frame of an animation) and applies its EXIF orientation.
pub fn decode(bytes: &[u8]) -> Result<Decoded, MediaError> {
    let reader = reader(bytes)?;
    let format = reader.format().ok_or(MediaError::Unsupported)?;
    let mut decoder = reader.into_decoder().map_err(MediaError::from_image)?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder).map_err(MediaError::from_image)?;
    image.apply_orientation(orientation);
    Ok(Decoded { image, format })
}

/// Decodes `bytes` once and computes dimensions, blurhash and thumbnail.
pub fn derive(bytes: &[u8]) -> Result<Derived, MediaError> {
    let d = decode(bytes)?;
    Ok(Derived {
        width: d.width(),
        height: d.height(),
        blurhash: d.blurhash()?,
        thumbnail: d.thumbnail(THUMBNAIL_EDGE)?,
    })
}

/// Renders `hash` as `width` × `height` RGBA pixels (each edge clamped to 1…256) for a
/// placeholder.
pub fn blurhash_rgba(hash: &str, width: u32, height: u32) -> Result<Vec<u8>, MediaError> {
    let (w, h) = (
        width.clamp(1, MAX_PLACEHOLDER_EDGE),
        height.clamp(1, MAX_PLACEHOLDER_EDGE),
    );
    blurhash::decode(hash, w, h, 1.0).map_err(|e| MediaError::Blurhash(e.to_string()))
}

impl Decoded {
    /// Width in pixels.
    pub fn width(&self) -> u32 {
        self.image.width()
    }

    /// Height in pixels.
    pub fn height(&self) -> u32 {
        self.image.height()
    }

    /// Whether the pixels carry alpha.
    pub fn has_alpha(&self) -> bool {
        self.image.color().has_alpha()
    }

    /// MIME type of the source container.
    pub fn mime(&self) -> &'static str {
        self.format.to_mime_type()
    }

    /// The blurhash of the image (4 × 3 components, 3 × 4 for portrait images).
    pub fn blurhash(&self) -> Result<String, MediaError> {
        let small = self
            .image
            .thumbnail(BLURHASH_SOURCE_EDGE, BLURHASH_SOURCE_EDGE)
            .to_rgba8();
        let (cx, cy) = if small.width() >= small.height() {
            (4, 3)
        } else {
            (3, 4)
        };
        blurhash::encode(cx, cy, small.width(), small.height(), small.as_raw())
            .map_err(|e| MediaError::Blurhash(e.to_string()))
    }

    /// A thumbnail whose longest edge is at most `max_edge` (never upscaled): PNG when the image
    /// has alpha (stickers), JPEG otherwise.
    pub fn thumbnail(&self, max_edge: u32) -> Result<Encoded, MediaError> {
        let small = self.fit(max_edge);
        if self.has_alpha() {
            encode_png(&small)
        } else {
            encode_jpeg(&flatten(&small), THUMBNAIL_QUALITY)
        }
    }

    /// Re-encodes as JPEG at `quality` (0.0 … 1.0, like a canvas), scaled down so the longest
    /// edge is at most `max_edge` (never upscaled). Alpha is composed onto white. No metadata is
    /// written.
    pub fn encode_jpeg(&self, max_edge: Option<u32>, quality: f32) -> Result<Encoded, MediaError> {
        let img = match max_edge {
            Some(edge) => self.fit(edge),
            None => self.image.clone(),
        };
        encode_jpeg(&flatten(&img), quality_byte(quality))
    }

    /// Re-encodes at full size in a lossless or near-lossless form of its own kind: PNG stays
    /// PNG; images with alpha become PNG; everything else becomes JPEG at quality 0.92 (a
    /// canvas's default). No metadata is written.
    pub fn reencode(&self) -> Result<Encoded, MediaError> {
        if self.format == ImageFormat::Png || self.has_alpha() {
            encode_png(&self.image)
        } else {
            encode_jpeg(&flatten(&self.image), 92)
        }
    }

    fn fit(&self, max_edge: u32) -> DynamicImage {
        let edge = max_edge.max(1);
        if self.width() <= edge && self.height() <= edge {
            return self.image.clone();
        }
        self.image.resize(edge, edge, FilterType::Triangle)
    }
}

/// Composes alpha onto white and drops it.
fn flatten(img: &DynamicImage) -> RgbImage {
    if !img.color().has_alpha() {
        return img.to_rgb8();
    }
    let rgba = img.to_rgba8();
    let mut out = RgbImage::new(rgba.width(), rgba.height());
    for (src, dst) in rgba.pixels().zip(out.pixels_mut()) {
        let a = u32::from(src[3]);
        for c in 0..3 {
            let v = (u32::from(src[c]) * a + 255 * (255 - a)) / 255;
            dst[c] = u8::try_from(v).unwrap_or(u8::MAX);
        }
    }
    out
}

fn quality_byte(q: f32) -> u8 {
    let q = if q.is_finite() {
        q.clamp(0.01, 1.0)
    } else {
        0.92
    };
    // 0.0 … 1.0 → 1 … 100; the cast is exact after the clamp.
    (q * 100.0).round().clamp(1.0, 100.0) as u8
}

pub(crate) fn encode_jpeg(img: &RgbImage, quality: u8) -> Result<Encoded, MediaError> {
    let mut bytes = Vec::new();
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, quality.max(1));
    img.write_with_encoder(enc)
        .map_err(MediaError::from_image)?;
    Ok(Encoded {
        bytes,
        mime: "image/jpeg",
        width: img.width(),
        height: img.height(),
    })
}

pub(crate) fn encode_png(img: &DynamicImage) -> Result<Encoded, MediaError> {
    let mut bytes = Vec::new();
    img.write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
        .map_err(MediaError::from_image)?;
    Ok(Encoded {
        bytes,
        mime: "image/png",
        width: img.width(),
        height: img.height(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SampleFormat, sample_image};

    #[test]
    fn derive_gives_dims_blurhash_and_a_small_thumbnail() {
        let jpeg = sample_image(1200, 800, 3, SampleFormat::Jpeg).unwrap();
        let info = probe(&jpeg).unwrap();
        assert_eq!(
            (info.width, info.height, info.mime),
            (1200, 800, "image/jpeg")
        );
        let d = derive(&jpeg).unwrap();
        assert_eq!((d.width, d.height), (1200, 800));
        assert_eq!((d.thumbnail.width, d.thumbnail.height), (400, 267));
        assert_eq!(d.thumbnail.mime, "image/jpeg");
        assert!(d.blurhash.len() >= 6, "{}", d.blurhash);
        // The thumbnail decodes, and so does the hash.
        assert_eq!(decode(&d.thumbnail.bytes).unwrap().width(), 400);
        assert_eq!(
            blurhash_rgba(&d.blurhash, 32, 20).unwrap().len(),
            32 * 20 * 4
        );
        // Deterministic.
        assert_eq!(derive(&jpeg).unwrap(), d);
    }

    #[test]
    fn small_images_are_never_upscaled_and_alpha_stays_png() {
        let png = sample_image(120, 90, 1, SampleFormat::PngAlpha).unwrap();
        let d = decode(&png).unwrap();
        assert!(d.has_alpha());
        let t = d.thumbnail(THUMBNAIL_EDGE).unwrap();
        assert_eq!((t.width, t.height, t.mime), (120, 90, "image/png"));
        let j = d.encode_jpeg(Some(1600), 0.7).unwrap();
        assert_eq!((j.width, j.height, j.mime), (120, 90, "image/jpeg"));
    }

    #[test]
    fn garbage_and_bombs_are_refused() {
        assert!(matches!(
            decode(b"definitely not"),
            Err(MediaError::Unsupported)
        ));
        assert!(matches!(
            decode(&[0xff, 0xd8, 0xff, 0xe0, 0, 4, 0, 0]),
            Err(MediaError::Decode(_) | MediaError::Unsupported)
        ));
        // A PNG header claiming 100000 × 100000 pixels.
        let mut bomb = sample_image(8, 8, 1, SampleFormat::Png).unwrap();
        bomb[16..20].copy_from_slice(&100_000u32.to_be_bytes());
        bomb[20..24].copy_from_slice(&100_000u32.to_be_bytes());
        assert!(decode(&bomb).is_err());
        assert!(blurhash_rgba("??", 4, 4).is_err());
    }

    #[test]
    fn quality_maps_like_a_canvas() {
        assert_eq!(quality_byte(0.7), 70);
        assert_eq!(quality_byte(2.0), 100);
        assert_eq!(quality_byte(f32::NAN), 92);
        assert_eq!(quality_byte(0.0), 1);
    }
}
