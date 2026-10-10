//! Deterministic synthetic pictures for fakes, fixtures and tests (no real photos).

use std::io::Cursor;

use image::{DynamicImage, ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};

use crate::MediaError;
use crate::decode::encode_jpeg;

/// Container of a [`sample_image`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SampleFormat {
    /// JPEG, quality 85.
    Jpeg,
    /// Opaque PNG.
    Png,
    /// PNG with a transparent border (a sticker-like image).
    PngAlpha,
    /// A single-frame GIF.
    Gif,
}

impl SampleFormat {
    /// The MIME type of the container.
    pub fn mime(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png | Self::PngAlpha => "image/png",
            Self::Gif => "image/gif",
        }
    }
}

/// SplitMix64.
fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn byte(v: u64, shift: u32) -> u8 {
    u8::try_from((v >> shift) & 0xff).unwrap_or(0)
}

/// A `width` × `height` picture (each edge clamped to 1…4096): a two-colour gradient with a few
/// discs, all chosen by `seed`. The same arguments always give the same bytes.
pub fn sample_image(
    width: u32,
    height: u32,
    seed: u64,
    format: SampleFormat,
) -> Result<Vec<u8>, MediaError> {
    let (w, h) = (width.clamp(1, 4096), height.clamp(1, 4096));
    let r = mix(seed);
    let (a, b) = (
        [byte(r, 0), byte(r, 8), byte(r, 16)],
        [byte(r, 24), byte(r, 32), byte(r, 40)],
    );
    let discs: Vec<(i64, i64, i64, [u8; 3])> = (0..4u64)
        .map(|i| {
            let d = mix(seed ^ (i + 1).wrapping_mul(0x5851_f42d_4c95_7f2d));
            let cx = i64::try_from(d % u64::from(w)).unwrap_or(0);
            let cy = i64::try_from((d >> 16) % u64::from(h)).unwrap_or(0);
            let rad = i64::from(w.min(h) / 8 + 1) + i64::try_from((d >> 32) % 32).unwrap_or(0);
            (cx, cy, rad, [byte(d, 40), byte(d, 48), byte(d, 56)])
        })
        .collect();
    let mut img = RgbImage::new(w, h);
    for (x, y, px) in img.enumerate_pixels_mut() {
        let t = u32::try_from((u64::from(x) + u64::from(y)) * 255 / u64::from(w + h).max(1))
            .unwrap_or(0);
        let mut c = [0u8; 3];
        for i in 0..3 {
            let v = (u32::from(a[i]) * (255 - t) + u32::from(b[i]) * t) / 255;
            c[i] = u8::try_from(v).unwrap_or(u8::MAX);
        }
        for (cx, cy, rad, color) in &discs {
            let (dx, dy) = (i64::from(x) - cx, i64::from(y) - cy);
            if dx * dx + dy * dy <= rad * rad {
                c = *color;
            }
        }
        // A little texture so JPEG output is not trivially compressible.
        let n = byte(mix(seed ^ (u64::from(x) << 32 | u64::from(y))), 0) / 16;
        *px = Rgb([
            c[0].saturating_add(n),
            c[1].saturating_add(n),
            c[2].saturating_add(n),
        ]);
    }
    match format {
        SampleFormat::Jpeg => Ok(encode_jpeg(&img, 85)?.bytes),
        SampleFormat::Png => write(&DynamicImage::ImageRgb8(img), ImageFormat::Png),
        SampleFormat::Gif => write(&DynamicImage::ImageRgb8(img), ImageFormat::Gif),
        SampleFormat::PngAlpha => {
            let border = (w.min(h) / 10).max(1);
            let rgba = RgbaImage::from_fn(w, h, |x, y| {
                let p = img.get_pixel(x, y);
                let inside = x >= border && y >= border && x + border < w && y + border < h;
                Rgba([p[0], p[1], p[2], if inside { 255 } else { 0 }])
            });
            write(&DynamicImage::ImageRgba8(rgba), ImageFormat::Png)
        }
    }
}

fn write(img: &DynamicImage, format: ImageFormat) -> Result<Vec<u8>, MediaError> {
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), format)
        .map_err(MediaError::from_image)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe;

    #[test]
    fn samples_are_deterministic_and_decodable() {
        for f in [
            SampleFormat::Jpeg,
            SampleFormat::Png,
            SampleFormat::PngAlpha,
            SampleFormat::Gif,
        ] {
            let a = sample_image(64, 48, 7, f).unwrap();
            assert_eq!(a, sample_image(64, 48, 7, f).unwrap());
            assert_ne!(a, sample_image(64, 48, 8, f).unwrap());
            let info = probe(&a).unwrap();
            assert_eq!((info.width, info.height, info.mime), (64, 48, f.mime()));
        }
    }
}
