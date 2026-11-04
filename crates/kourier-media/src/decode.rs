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
/// A picture whose longest edge is more than this many times the thumbnail's is shrunk in two
/// steps (see [`Decoded::fit_thumbnail`]).
const TWO_STEP_RATIO: u32 = 4;

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

/// Decodes `bytes` once and computes dimensions, blurhash and thumbnail. The picture is fitted to
/// the thumbnail's size once and both come from that: a blurhash keeps a handful of components,
/// so it never needs the full-size pixels (Signal Desktop hashes a canvas of at most 200 px,
/// Android a decode at 1/16 of the size).
pub fn derive(bytes: &[u8]) -> Result<Derived, MediaError> {
    let d = decode(bytes)?;
    let small = d.fit_thumbnail(THUMBNAIL_EDGE);
    Ok(Derived {
        width: d.width(),
        height: d.height(),
        blurhash: blurhash_of(&small)?,
        thumbnail: d.encode_thumbnail(&small)?,
    })
}

/// The blurhash of `picture` (4 × 3 components, 3 × 4 for portrait pictures) from an area
/// average of at most [`BLURHASH_SOURCE_EDGE`] px.
fn blurhash_of(picture: &DynamicImage) -> Result<String, MediaError> {
    let small = picture
        .thumbnail(BLURHASH_SOURCE_EDGE, BLURHASH_SOURCE_EDGE)
        .to_rgba8();
    note(
        "blurhash",
        (picture.width(), picture.height()),
        (small.width(), small.height()),
    );
    let (cx, cy) = if small.width() >= small.height() {
        (4, 3)
    } else {
        (3, 4)
    };
    blurhash::encode(cx, cy, small.width(), small.height(), small.as_raw())
        .map_err(|e| MediaError::Blurhash(e.to_string()))
}

/// Records a step a picture goes through (what, the size it started from, the size it made) for
/// the tests that look at WHICH pixels a result is made from; nothing in a normal build.
#[cfg(test)]
fn note(what: &'static str, from: (u32, u32), to: (u32, u32)) {
    STEPS.with(|steps| steps.borrow_mut().push((what, from, to)));
}
#[cfg(not(test))]
fn note(_what: &'static str, _from: (u32, u32), _to: (u32, u32)) {}

/// One recorded step: (what, from, to).
#[cfg(test)]
type Step = (&'static str, (u32, u32), (u32, u32));

#[cfg(test)]
thread_local! {
    static STEPS: std::cell::RefCell<Vec<Step>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The steps recorded on this thread since the last call.
#[cfg(test)]
fn take_steps() -> Vec<Step> {
    STEPS.with(|steps| std::mem::take(&mut *steps.borrow_mut()))
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

    /// The blurhash of the image (4 × 3 components, 3 × 4 for portrait images), taken from the
    /// picture fitted to [`THUMBNAIL_EDGE`] like the one [`derive`] makes.
    pub fn blurhash(&self) -> Result<String, MediaError> {
        blurhash_of(&self.fit_thumbnail(THUMBNAIL_EDGE))
    }

    /// A thumbnail whose longest edge is at most `max_edge` (never upscaled): PNG when the image
    /// has alpha (stickers), JPEG otherwise.
    pub fn thumbnail(&self, max_edge: u32) -> Result<Encoded, MediaError> {
        self.encode_thumbnail(&self.fit_thumbnail(max_edge))
    }

    /// `small`, a fit of this image, as a thumbnail file.
    fn encode_thumbnail(&self, small: &DynamicImage) -> Result<Encoded, MediaError> {
        if self.has_alpha() {
            encode_png(small)
        } else {
            encode_jpeg(&flatten(small), THUMBNAIL_QUALITY)
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

    /// The picture for a thumbnail or a blurhash: [`Self::fit`]'s size, made cheaper for a big
    /// original. A picture whose longest edge is more than [`TWO_STEP_RATIO`] times `max_edge`
    /// is first averaged by area down to twice `max_edge` (one pass over the pixels, the
    /// "shrink" of libvips' shrink-then-reduce) and then fitted by the same triangle filter to
    /// the EXACT size the one-step fit gives, so a 12 MP photo costs about a fifth of the time
    /// and the thumbnail keeps its size and look. Smaller pictures, and any picture with alpha
    /// (an area average that ignores alpha bleeds colour out of the transparent part), take the
    /// one-step fit. Thumbnails and blurhashes come from here, and both are sent: the quote
    /// thumbnail of a reply to a picture (uploaded as it is) and the `blurHash` of every picture's
    /// pointer. They are small derived previews that the other side shows as placeholders, so a
    /// level or two of difference against the one-step fit is accepted (T2, the lead's decision;
    /// 54.7 dB against it on a 12 MP photo). The picture itself, the attachment Kourier SENDS,
    /// is cut by [`Self::fit`] (through `encode_jpeg`, `scale_to_level`) and stays byte-identical.
    fn fit_thumbnail(&self, max_edge: u32) -> DynamicImage {
        let edge = max_edge.max(1);
        let (w, h) = (self.width(), self.height());
        let longest = u64::from(w.max(h));
        if self.has_alpha() || longest <= u64::from(edge) * u64::from(TWO_STEP_RATIO) {
            return self.fit(edge);
        }
        let (width, height) = fitted_size(w, h, edge);
        let half = self
            .image
            .thumbnail(edge.saturating_mul(2), edge.saturating_mul(2));
        note("area", (w, h), (half.width(), half.height()));
        let out = half.resize_exact(width, height, FilterType::Triangle);
        note("triangle", (half.width(), half.height()), (width, height));
        out
    }

    fn fit(&self, max_edge: u32) -> DynamicImage {
        let edge = max_edge.max(1);
        if self.width() <= edge && self.height() <= edge {
            return self.image.clone();
        }
        let out = self.image.resize(edge, edge, FilterType::Triangle);
        note(
            "triangle",
            (self.width(), self.height()),
            (out.width(), out.height()),
        );
        out
    }
}

/// The size `DynamicImage::resize(edge, edge, _)` gives a `width` × `height` picture: `image`
/// keeps this rule private and a test holds the copy to the real thing.
fn fitted_size(width: u32, height: u32, edge: u32) -> (u32, u32) {
    let ratio = (f64::from(edge) / f64::from(width)).min(f64::from(edge) / f64::from(height));
    let side = |n: u32| (f64::from(n) * ratio).round().max(1.0) as u32;
    (side(width), side(height))
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
    use crate::{
        MIN_DIMENSIONS, SCALABLE_DIMENSIONS, SampleFormat, ScaleLevel, sample_image, scale_to_level,
    };

    /// The lowest PSNR (dB) the two-step fit may have against the one-step fit of a photo-like
    /// picture (shading, a hard edge, grain, fine stripes). Measured 2026-10-14: 54.7 dB; a
    /// box-only shrink 49.4 dB (same look, so it is the step log that holds the method); a
    /// nearest-neighbour last step 34.9 dB.
    const PSNR_FLOOR: f64 = 45.0;

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

    /// What `f` does, and the steps it recorded as (what, from, to).
    fn steps_of<T>(f: impl FnOnce() -> T) -> (T, Vec<Step>) {
        take_steps();
        let out = f();
        (out, take_steps())
    }

    /// The blurhash as `blurhash()` made it before T2: from an area average of the FULL picture.
    fn blurhash_of_the_full_picture(d: &Decoded) -> String {
        let small = d
            .image
            .thumbnail(BLURHASH_SOURCE_EDGE, BLURHASH_SOURCE_EDGE)
            .to_rgba8();
        let (cx, cy) = if small.width() >= small.height() {
            (4, 3)
        } else {
            (3, 4)
        };
        blurhash::encode(cx, cy, small.width(), small.height(), small.as_raw()).unwrap()
    }

    #[test]
    fn the_blurhash_is_made_from_the_picture_fitted_to_the_thumbnail() {
        let jpeg = sample_image(2000, 1500, 5, SampleFormat::Jpeg).unwrap();
        let (derived, steps) = steps_of(|| derive(&jpeg).unwrap());
        // The picture is fitted once, and the 64 px average starts from the fit's result, not
        // from the 2000 × 1500 pixels.
        let fits: Vec<_> = steps.iter().filter(|s| s.0 == "triangle").collect();
        assert_eq!(fits.len(), 1, "one fit for both outputs: {steps:?}");
        let hashes: Vec<_> = steps.iter().filter(|s| s.0 == "blurhash").collect();
        assert_eq!(hashes.len(), 1, "{steps:?}");
        assert_eq!(
            hashes[0].1, fits[0].2,
            "the hash's source is the fit: {steps:?}"
        );
        assert!(
            hashes[0].1.0.max(hashes[0].1.1) <= THUMBNAIL_EDGE,
            "{steps:?}"
        );
        assert_eq!(
            (derived.thumbnail.width, derived.thumbnail.height),
            (400, 300)
        );
        // `Decoded::blurhash` is the same rule, so the two agree.
        let d = decode(&jpeg).unwrap();
        let (hash, steps) = steps_of(|| d.blurhash().unwrap());
        assert_eq!(hash, derived.blurhash);
        let hashes: Vec<_> = steps.iter().filter(|s| s.0 == "blurhash").collect();
        assert_eq!(hashes.len(), 1, "{steps:?}");
        assert!(
            hashes[0].1.0.max(hashes[0].1.1) <= THUMBNAIL_EDGE,
            "{steps:?}"
        );
    }

    #[test]
    fn a_blurhash_from_the_fit_looks_like_the_one_from_the_full_picture() {
        // A picture that fits the thumbnail is not fitted: the hash is what it always was.
        let small = decode(&sample_image(300, 200, 7, SampleFormat::Jpeg).unwrap()).unwrap();
        assert_eq!(
            small.blurhash().unwrap(),
            blurhash_of_the_full_picture(&small)
        );
        // A big one: the strings may differ, the placeholders they draw hardly do.
        let big = decode(&sample_image(2000, 1500, 7, SampleFormat::Jpeg).unwrap()).unwrap();
        let (new, old) = (big.blurhash().unwrap(), blurhash_of_the_full_picture(&big));
        let (a, b) = (
            blurhash_rgba(&new, 32, 24).unwrap(),
            blurhash_rgba(&old, 32, 24).unwrap(),
        );
        let mean = a
            .iter()
            .zip(&b)
            .map(|(x, y)| u64::from(x.abs_diff(*y)))
            .sum::<u64>() as f64
            / a.len() as f64;
        println!("placeholder difference (mean per byte): {mean:.3}");
        assert!(
            mean <= 2.0,
            "the placeholder changed by {mean} levels on average"
        );
    }

    /// A `width` × `height` picture without content worth the name (a gradient): for the tests
    /// of sizes, which never look at the pixels.
    fn plain(width: u32, height: u32) -> Decoded {
        let image = RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x % 251) as u8, (y % 241) as u8, ((x + y) % 239) as u8])
        });
        Decoded {
            image: DynamicImage::ImageRgb8(image),
            format: ImageFormat::Jpeg,
        }
    }

    /// Peak signal-to-noise ratio between two pictures of one size (dB; 8 bits, all channels).
    fn psnr(a: &DynamicImage, b: &DynamicImage) -> f64 {
        assert_eq!((a.width(), a.height()), (b.width(), b.height()));
        let (a, b) = (a.to_rgb8(), b.to_rgb8());
        let squares: u64 = a
            .as_raw()
            .iter()
            .zip(b.as_raw())
            .map(|(x, y)| u64::from(x.abs_diff(*y)).pow(2))
            .sum();
        let mse = squares as f64 / a.as_raw().len() as f64;
        if mse == 0.0 {
            return f64::INFINITY;
        }
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }

    #[test]
    fn a_big_original_is_shrunk_in_two_steps_for_a_thumbnail() {
        let jpeg = sample_image(4000, 3000, 3, SampleFormat::Jpeg).unwrap();
        let big = decode(&jpeg).unwrap();
        let (small, steps) = steps_of(|| big.fit_thumbnail(THUMBNAIL_EDGE));
        assert_eq!((small.width(), small.height()), (400, 300));
        // An area average down to twice the thumbnail first; the triangle filter only has the
        // last factor of two to do (it was 4000 to 400 in one go before).
        assert_eq!(
            steps,
            vec![
                ("area", (4000, 3000), (800, 600)),
                ("triangle", (800, 600), (400, 300))
            ]
        );
        // The thumbnail and the blurhash of `derive` come from it.
        let (_, steps) = steps_of(|| derive(&jpeg));
        let triangles: Vec<_> = steps.iter().filter(|s| s.0 == "triangle").collect();
        assert_eq!(triangles.len(), 1, "{steps:?}");
        assert!(triangles[0].1.0 <= 2 * THUMBNAIL_EDGE, "{steps:?}");
        // So does `thumbnail`, and the blurhash of the decoded picture.
        let (_, steps) = steps_of(|| big.thumbnail(THUMBNAIL_EDGE).unwrap());
        assert!(steps.iter().any(|s| s.0 == "area"), "{steps:?}");
        let (_, steps) = steps_of(|| big.blurhash().unwrap());
        assert!(steps.iter().any(|s| s.0 == "area"), "{steps:?}");
    }

    #[test]
    fn up_to_four_times_the_thumbnail_there_is_one_step() {
        for (w, h) in [(1200, 800), (1600, 1200), (800, 1600), (1600, 1600)] {
            let d = plain(w, h);
            let (small, steps) = steps_of(|| d.fit_thumbnail(THUMBNAIL_EDGE));
            let to = (small.width(), small.height());
            assert_eq!(steps, vec![("triangle", (w, h), to)], "{w}x{h}");
        }
        // A picture of the thumbnail's size or less is not touched at all.
        let (_, steps) = steps_of(|| plain(300, 400).fit_thumbnail(THUMBNAIL_EDGE));
        assert!(steps.is_empty(), "{steps:?}");
        // One pixel above four times the edge it is two.
        let d = plain(1601, 1000);
        let (_, steps) = steps_of(|| d.fit_thumbnail(THUMBNAIL_EDGE));
        assert_eq!(
            steps.iter().map(|s| s.0).collect::<Vec<_>>(),
            ["area", "triangle"]
        );
    }

    #[test]
    fn a_picture_with_alpha_is_fitted_in_one_step_whatever_its_size() {
        // An area average that ignores alpha bleeds the colour of the transparent border in.
        let png = sample_image(2400, 1800, 2, SampleFormat::PngAlpha).unwrap();
        let sticker = decode(&png).unwrap();
        assert!(sticker.has_alpha());
        let (small, steps) = steps_of(|| sticker.fit_thumbnail(THUMBNAIL_EDGE));
        assert_eq!(
            steps,
            vec![("triangle", (2400, 1800), (small.width(), small.height()))]
        );
    }

    #[test]
    fn the_two_step_fit_has_the_size_of_the_one_step_fit() {
        let shapes = [
            (4000, 3000),
            (3000, 4000),
            (1080, 2340),
            (2340, 1080),
            (12000, 100),
            (100, 12000),
            (1601, 801),
            (1601, 1601),
            (4001, 3),
            (3, 4001),
            (16384, 1),
        ];
        for (w, h) in shapes {
            let d = plain(w, h);
            let one = d
                .image
                .resize(THUMBNAIL_EDGE, THUMBNAIL_EDGE, FilterType::Triangle);
            let two = d.fit_thumbnail(THUMBNAIL_EDGE);
            let (one, two) = ((one.width(), one.height()), (two.width(), two.height()));
            assert_eq!(two, one, "{w}x{h}");
            assert_eq!(fitted_size(w, h, THUMBNAIL_EDGE), one, "{w}x{h}");
        }
        // The same for other edges (the rule is relative to the edge asked for).
        for (w, h, edge) in [
            (1000, 700, 100),
            (999, 333, 50),
            (5000, 4999, 1200),
            (700, 7, 1),
        ] {
            let d = plain(w, h);
            let one = d.image.resize(edge, edge, FilterType::Triangle);
            let two = d.fit_thumbnail(edge);
            let (one, two) = ((one.width(), one.height()), (two.width(), two.height()));
            assert_eq!(two, one, "{w}x{h} at {edge}");
        }
    }

    /// A picture with what photos have and the gradients of [`plain`] do not: smooth shading,
    /// hard edges (a disc), grain, and fine texture (stripes a few pixels wide).
    fn photo_like(width: u32, height: u32) -> Decoded {
        let (w, h) = (i64::from(width), i64::from(height));
        let image = RgbImage::from_fn(width, height, |x, y| {
            let mut n = x.wrapping_mul(73_856_093) ^ y.wrapping_mul(19_349_663);
            n ^= n >> 13;
            n = n.wrapping_mul(0x5bd1_e995);
            let grain = i64::from((n >> 24) & 31) - 16;
            let (px, py) = (i64::from(x), i64::from(y));
            let (dx, dy) = (px - w / 3, py - h / 2);
            let mut rgb = if dx * dx + dy * dy < (h / 4) * (h / 4) {
                [230, 60 + 255 * px / w / 4, 40]
            } else {
                [255 * px / w, 255 * py / h, 255 - 255 * (px + py) / (w + h)]
            };
            if px > 3 * w / 4 {
                // Fine texture: stripes 3 px wide that run 5 px up for each 3 px across.
                let stripe = if (px / 3 + py / 5) % 2 == 0 { 40 } else { -40 };
                rgb = rgb.map(|c| c + stripe);
            }
            image::Rgb(rgb.map(|c| (c + grain).clamp(0, 255) as u8))
        });
        Decoded {
            image: DynamicImage::ImageRgb8(image),
            format: ImageFormat::Jpeg,
        }
    }

    #[test]
    fn the_two_step_fit_looks_like_the_one_step_fit() {
        let big = photo_like(4000, 3000);
        let one = big
            .image
            .resize(THUMBNAIL_EDGE, THUMBNAIL_EDGE, FilterType::Triangle);
        let two = big.fit_thumbnail(THUMBNAIL_EDGE);
        let db = psnr(&one, &two);
        println!("two-step against one-step: {db:.2} dB");
        assert!(db >= PSNR_FLOOR, "{db:.2} dB");
    }

    /// The pictures Kourier SENDS are cut by the one-step fit, as before T2: the area pre-step
    /// belongs to thumbnails only. A picture 8000 px wide is five times 1600 (level one's first
    /// edge) and one 16000 px wide more than five times 3072 (level three's), where a pre-step
    /// in `fit` would show; the last resort, 512 px, is checked on the first. Only the longest
    /// edge decides the pre-step, so the pictures are strips (8000 x 1200, 16000 x 800): five
    /// times fewer pixels than the 8000 x 6000 and 16000 x 4000 first used, which made this the
    /// longest test of the quick tier (13 s alone, 14.5 s in the run; R8 of the T2 fix round).
    /// That the strips still tell is asserted: `fit_thumbnail` at the same edge DOES take the
    /// area step on them.
    #[test]
    fn what_is_sent_is_cut_by_the_one_step_fit() {
        for (w, h, level, edge) in [
            (8000, 1200, ScaleLevel::ONE, 1600),
            (16000, 800, ScaleLevel::THREE, 3072),
        ] {
            assert_eq!(
                SCALABLE_DIMENSIONS
                    .into_iter()
                    .find(|e| *e <= level.max_dimensions),
                Some(edge),
                "the first edge the level tries"
            );
            assert!(w > edge * TWO_STEP_RATIO, "{w}x{h}: a pre-step would apply");
            let original = encode_jpeg(&plain(w, h).image.to_rgb8(), 85).unwrap().bytes;
            assert!(
                original.len() as u64 > level.threshold_size,
                "{w}x{h}: big enough to be scaled, not kept"
            );
            let d = decode(&original).unwrap();
            // The strip is one a thumbnail takes the area step for.
            let (_, thumb) = steps_of(|| d.fit_thumbnail(edge));
            let what: Vec<_> = thumb.iter().map(|s| s.0).collect();
            assert_eq!(what, ["area", "triangle"], "{w}x{h}: the thumbnail's steps");
            let cut = |edge: u32| {
                let reference = d.image.resize(edge, edge, FilterType::Triangle);
                encode_jpeg(&reference.to_rgb8(), quality_byte(level.quality)).unwrap()
            };
            // What a send makes of it: the first edge fits the budget, so it is the answer.
            let expected = cut(edge);
            assert!(expected.bytes.len() as u64 <= level.target_size);
            let (sent, steps) = steps_of(|| scale_to_level(&original, level).unwrap());
            assert_eq!(sent, expected, "{w}x{h}, {level:?}");
            let what: Vec<_> = steps.iter().map(|s| s.0).collect();
            assert_eq!(what, ["triangle"], "{w}x{h}: one step");
            if w == 8000 {
                let last_resort = d.encode_jpeg(Some(MIN_DIMENSIONS), level.quality).unwrap();
                assert_eq!(last_resort, cut(MIN_DIMENSIONS), "{w}x{h} at 512");
            }
        }
    }

    /// Where the time of a derive goes (`cargo test --release -p kourier-media -- --ignored
    /// --nocapture derive_cost`): a 12 MP photo, the best of five runs of each step.
    #[test]
    #[ignore = "a measurement, run by hand"]
    fn derive_cost() {
        let jpeg = sample_image(4096, 3072, 5, SampleFormat::Jpeg).unwrap();
        let best = |mut f: Box<dyn FnMut() + '_>| {
            (0..5)
                .map(|_| {
                    let t = std::time::Instant::now();
                    f();
                    t.elapsed().as_secs_f64() * 1000.0
                })
                .fold(f64::INFINITY, f64::min)
        };
        let d = decode(&jpeg).unwrap();
        let small = d.fit_thumbnail(THUMBNAIL_EDGE);
        let half = d.image.thumbnail(2 * THUMBNAIL_EDGE, 2 * THUMBNAIL_EDGE);
        let rows = [
            ("decode", best(Box::new(|| drop(decode(&jpeg).unwrap())))),
            (
                "one-step fit (sent pictures)",
                best(Box::new(|| drop(d.fit(THUMBNAIL_EDGE)))),
            ),
            (
                "two-step fit (thumbnails)",
                best(Box::new(|| drop(d.fit_thumbnail(THUMBNAIL_EDGE)))),
            ),
            (
                "  of it: the area pre-step",
                best(Box::new(|| {
                    drop(d.image.thumbnail(2 * THUMBNAIL_EDGE, 2 * THUMBNAIL_EDGE))
                })),
            ),
            (
                "  of it: the last triangle step",
                best(Box::new(|| {
                    drop(half.resize_exact(
                        THUMBNAIL_EDGE,
                        THUMBNAIL_EDGE * 3 / 4,
                        FilterType::Triangle,
                    ))
                })),
            ),
            (
                "blurhash from the full picture",
                best(Box::new(|| drop(blurhash_of(&d.image).unwrap()))),
            ),
            (
                "blurhash from the fit",
                best(Box::new(|| drop(blurhash_of(&small).unwrap()))),
            ),
            (
                "encode the thumbnail",
                best(Box::new(|| {
                    drop(encode_jpeg(&flatten(&small), THUMBNAIL_QUALITY).unwrap())
                })),
            ),
            (
                "derive (all of it)",
                best(Box::new(|| drop(derive(&jpeg).unwrap()))),
            ),
        ];
        for (what, ms) in rows {
            println!("derive_cost: {what:<32} {ms:8.1} ms");
        }
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
