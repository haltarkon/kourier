//! The per-network re-encode helper for outgoing images (§5.8): Signal Desktop's
//! `scaleImageToLevel` (`SD/ts/util/scaleImageToLevel.preload.ts:25-53, 109-175`).

use crate::decode::decode;
use crate::{Encoded, MediaError};

/// Edges tried from large to small until the JPEG fits the level's size budget.
pub const SCALABLE_DIMENSIONS: [u32; 5] = [3072, 2048, 1600, 1024, 768];
/// The last resort edge when no scalable dimension fits.
pub const MIN_DIMENSIONS: u32 = 512;

const MIB: u64 = 1024 * 1024;

/// A media quality level: Signal's levels one to three.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScaleLevel {
    /// Longest edge allowed.
    pub max_dimensions: u32,
    /// JPEG quality (0.0 … 1.0).
    pub quality: f32,
    /// Size budget of the re-encoded file (bytes).
    pub target_size: u64,
    /// Inputs up to this size are re-encoded at full size in their own kind.
    pub threshold_size: u64,
}

impl ScaleLevel {
    /// Level one, the default: 1600 px, quality 0.7, ≤ 1 MiB, threshold 0.2 MiB.
    pub const ONE: Self = Self {
        max_dimensions: 1600,
        quality: 0.7,
        target_size: MIB,
        threshold_size: MIB / 5,
    };
    /// Level two: 2048 px, quality 0.75, ≤ 1.5 MiB, threshold 0.3 MiB.
    pub const TWO: Self = Self {
        max_dimensions: 2048,
        quality: 0.75,
        target_size: MIB * 3 / 2,
        threshold_size: MIB * 3 / 10,
    };
    /// Level three ("high quality"): 4096 px, quality 0.75, ≤ 3 MiB, threshold 0.4 MiB.
    pub const THREE: Self = Self {
        max_dimensions: 4096,
        quality: 0.75,
        target_size: MIB * 3,
        threshold_size: MIB * 2 / 5,
    };
}

impl Default for ScaleLevel {
    fn default() -> Self {
        Self::ONE
    }
}

/// Re-encodes an outgoing still image for `level`, with its orientation applied and every
/// metadata block (EXIF, GPS, XMP) dropped. Inputs up to `threshold_size` keep their size and kind
/// ([`Decoded::reencode`](crate::Decoded::reencode)); larger ones become JPEGs at the first edge
/// of [`SCALABLE_DIMENSIONS`] (≤ `max_dimensions`) whose file fits `target_size`, else at
/// [`MIN_DIMENSIONS`]. Images are never upscaled. GIFs are refused with
/// [`MediaError::Unsupported`]: animations are sent unchanged.
pub fn scale_to_level(bytes: &[u8], level: ScaleLevel) -> Result<Encoded, MediaError> {
    let d = decode(bytes)?;
    if d.mime() == "image/gif" {
        return Err(MediaError::Unsupported);
    }
    let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if size <= level.threshold_size {
        return d.reencode();
    }
    for edge in SCALABLE_DIMENSIONS
        .into_iter()
        .filter(|e| *e <= level.max_dimensions)
    {
        let e = d.encode_jpeg(Some(edge), level.quality)?;
        if u64::try_from(e.bytes.len()).unwrap_or(u64::MAX) <= level.target_size {
            return Ok(e);
        }
    }
    d.encode_jpeg(Some(MIN_DIMENSIONS), level.quality)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SampleFormat, sample_image};

    #[test]
    fn large_images_shrink_to_1600_px_jpeg() {
        let png = sample_image(2400, 1800, 9, SampleFormat::Png).unwrap();
        assert!(png.len() as u64 > ScaleLevel::ONE.threshold_size);
        let out = scale_to_level(&png, ScaleLevel::ONE).unwrap();
        assert_eq!(out.mime, "image/jpeg");
        assert_eq!((out.width, out.height), (1600, 1200));
        assert!(out.bytes.len() as u64 <= ScaleLevel::ONE.target_size);
        // No EXIF, no XMP in the output: a bare JFIF stream.
        assert!(!out.bytes.windows(4).any(|w| w == b"Exif"));
    }

    #[test]
    fn a_tight_budget_walks_down_the_edges() {
        let jpeg = sample_image(2000, 1000, 2, SampleFormat::Jpeg).unwrap();
        let level = ScaleLevel {
            target_size: 1,
            threshold_size: 0,
            ..ScaleLevel::ONE
        };
        let out = scale_to_level(&jpeg, level).unwrap();
        assert_eq!((out.width, out.height), (MIN_DIMENSIONS, 256));
    }

    #[test]
    fn small_inputs_keep_size_and_kind_and_gifs_are_refused() {
        let png = sample_image(300, 200, 4, SampleFormat::Png).unwrap();
        let out = scale_to_level(&png, ScaleLevel::ONE).unwrap();
        assert_eq!((out.mime, out.width, out.height), ("image/png", 300, 200));
        let jpeg = sample_image(300, 200, 4, SampleFormat::Jpeg).unwrap();
        let out = scale_to_level(&jpeg, ScaleLevel::ONE).unwrap();
        assert_eq!((out.mime, out.width, out.height), ("image/jpeg", 300, 200));
        let gif = sample_image(30, 20, 4, SampleFormat::Gif).unwrap();
        assert!(matches!(
            scale_to_level(&gif, ScaleLevel::ONE),
            Err(MediaError::Unsupported)
        ));
    }
}
