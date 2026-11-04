//! Image decoding, resizing, metadata stripping, blurhash and thumbnails (docs/architecture.md
//! §5.8, §8.6). Pure Rust codecs (`image` with PNG, JPEG, GIF, WebP and BMP; `blurhash`); no Qt,
//! no protocol types, no I/O of its own: every function works on bytes in memory, except that the
//! look reads through a caller's `Read + Seek`.
//!
//! - [`decode`]: decode with EXIF orientation applied and decompression-bomb limits
//!   ([`MAX_DIMENSION`], [`MAX_ALLOC`]); [`probe`] reads dimensions without decoding pixels.
//! - [`derive`]: what the core stores for an image attachment: dimensions, a blurhash and a small
//!   JPEG/PNG thumbnail.
//! - [`scale_to_level`]: the per-network re-encode helper for backends, Signal Desktop's
//!   `scaleImageToLevel` (max 1600 px, quality 0.7, ≤ 1 MiB at level one;
//!   `SD/ts/util/scaleImageToLevel.preload.ts`). Re-encoding drops every metadata block.
//! - [`clean_still`]: the one cleaning function of a picture a network sends (LOC): re-encoded,
//!   else stripped, never returned with a location; [`CLEANED_TYPES`] is what it cleans.
//! - [`strip_metadata`]: removes EXIF, XMP, IPTC and text chunks from JPEG and PNG without
//!   re-encoding, unless an EXIF orientation must be applied first.
//! - [`blurhash_rgba`]: placeholder pixels for the UI while a thumbnail decodes.
//! - [`sniff_mime`], [`mime_from_extension`], [`extension_for_mime`]: content types.
//! - [`look`]: does a file carry where it was taken? Found / None / Unreadable, bounded, without
//!   decoding pixels or keeping a coordinate (LOC).
//! - [`sample_image`]: a deterministic synthetic picture for fakes, fixtures and tests;
//!   [`sample_located`]: synthetic files that carry a made-up location, for the LOC tests.
//!
//! Errors never quote input bytes, so they are log-safe (§9.1).

mod bmff;
mod chunks;
mod clean;
mod decode;
mod heif;
mod location;
mod mime;
mod movie;
mod sample;
mod scale;
mod strip;
mod tiff;
mod xmp;

pub use clean::{CLEANED_TYPES, NotCleaned, clean_still};
pub use decode::{
    Decoded, Derived, ImageInfo, MAX_ALLOC, MAX_DIMENSION, THUMBNAIL_EDGE, blurhash_rgba, decode,
    derive, probe,
};
pub use location::{
    Evidence, EvidenceKind, Look, LookFormat, ReadSeek, Unreadable, look, look_why,
};
pub use mime::{extension_for_mime, mime_from_extension, sniff_mime};
pub use sample::{SampleFormat, SampleLocated, sample_image, sample_located};
pub use scale::{MIN_DIMENSIONS, SCALABLE_DIMENSIONS, ScaleLevel, scale_to_level};
pub use strip::{Stripped, strip_metadata};

/// Errors of this crate. Log-safe: they never contain image bytes.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    /// The bytes are not an image format this build decodes (or not an image at all).
    #[error("unsupported or unrecognized image format")]
    Unsupported,
    /// The image exceeds [`MAX_DIMENSION`] or would need more than [`MAX_ALLOC`] bytes.
    #[error("the image is too large to decode")]
    TooLarge,
    /// The image is malformed.
    #[error("the image could not be decoded: {0}")]
    Decode(String),
    /// Encoding failed.
    #[error("the image could not be encoded: {0}")]
    Encode(String),
    /// A blurhash could not be computed or parsed.
    #[error("blurhash: {0}")]
    Blurhash(String),
}

impl MediaError {
    pub(crate) fn from_image(e: image::ImageError) -> Self {
        match e {
            image::ImageError::Limits(_) => Self::TooLarge,
            image::ImageError::Unsupported(_) => Self::Unsupported,
            image::ImageError::Encoding(e) => Self::Encode(e.to_string()),
            other => Self::Decode(other.to_string()),
        }
    }
}

/// Encoded image bytes with their type and size.
#[derive(Clone, PartialEq, Eq)]
pub struct Encoded {
    /// The file bytes.
    pub bytes: Vec<u8>,
    /// MIME type (`image/jpeg`, `image/png`).
    pub mime: &'static str,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl std::fmt::Debug for Encoded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the pixels.
        f.debug_struct("Encoded")
            .field("len", &self.bytes.len())
            .field("mime", &self.mime)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_bytes() {
        let e = Encoded {
            bytes: vec![1, 2, 3],
            mime: "image/png",
            width: 1,
            height: 1,
        };
        let s = format!("{e:?}");
        assert!(s.contains("len: 3") && !s.contains("[1, 2, 3]"), "{s}");
    }
}
