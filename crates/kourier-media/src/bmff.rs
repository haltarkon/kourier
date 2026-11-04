//! A bounded, read-lenient ISO-BMFF box walker (the JPEG XL container, HEIF and the movies of the
//! later steps). It reads box headers only; what a payload holds is the caller's business.
//!
//! Lenient where a read-only look can be (critic F12): no box has to come first, a box whose size
//! reaches past its container is cut at the container's end (a recording cut short), fewer than 8
//! bytes left at the end of a container end it (QuickTime's 32-bit zero terminator). Refused (the
//! walk answers [`Bound`]): a size that overflows, a size below its own header, a size of 2..7
//! (so every box moves the walk forward: no loop is possible), nesting deeper than
//! [`DEPTH_MAX`], and the bounds of the source (reads, headers).
//!
//! Headers are counted against [`crate::location::HEADERS_MAX`] through [`Bytes::count_header`],
//! except the boxes that are only ever skipped by their header ([`SKIPPED`]): a fragmented
//! movie of many hours has more `moof` + `mdat` pairs than the bound.

use crate::location::{Bound, Bytes, DEPTH_MAX, Window};
use crate::tiff;

/// Boxes skipped by their header alone, never counted: media data, fragments, padding, and the
/// JPEG XL codestream.
pub(crate) const SKIPPED: [[u8; 4]; 6] =
    [*b"mdat", *b"moof", *b"free", *b"skip", *b"jxlc", *b"jxlp"];

/// One box's header. `payload..end` is what follows the header (and the `uuid` type, if any).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BoxHeader {
    /// The four-character type.
    pub(crate) typ: [u8; 4],
    /// The extended type of a `uuid` box.
    pub(crate) uuid: Option<[u8; 16]>,
    /// Where the box starts.
    pub(crate) start: u64,
    /// Where its payload starts.
    pub(crate) payload: u64,
    /// Where it ends (cut at its container's end).
    pub(crate) end: u64,
    /// The nesting level of the box (top-level boxes are 0).
    pub(crate) depth: u32,
}

impl BoxHeader {
    /// The payload's length.
    pub(crate) fn payload_len(&self) -> u64 {
        self.end - self.payload
    }

    /// The payload as its own source (offset 0 = the payload's first byte).
    pub(crate) fn window<'a>(&self, src: &'a mut dyn Bytes) -> Window<'a> {
        Window {
            inner: src,
            base: self.payload,
            len: self.payload_len(),
        }
    }

    /// The children of this box, starting `skip` bytes into its payload (4 for a FullBox's version
    /// and flags). Deeper than [`DEPTH_MAX`] is [`Bound`].
    pub(crate) fn children_after(&self, skip: u64) -> Result<Boxes, Bound> {
        let depth = self.depth + 1;
        if depth > DEPTH_MAX {
            return Err(Bound);
        }
        let pos = self.payload.checked_add(skip).ok_or(Bound)?;
        if pos > self.end {
            return Err(Bound);
        }
        Ok(Boxes {
            pos,
            end: self.end,
            depth,
        })
    }

    /// The children of a plain container box.
    pub(crate) fn children(&self) -> Result<Boxes, Bound> {
        self.children_after(0)
    }

    /// The children of a `meta` box, which is a FullBox in ISO files (HEIF, iTunes `udta/meta`)
    /// but a plain container in QuickTime's `moov/meta` (critic F4). Detected by content, as ffmpeg
    /// and exiftool do: when the payload starts with a plausible child (a size of at least 8 and a
    /// known first child's type), there are no version and flags.
    pub(crate) fn meta_children(&self, src: &mut dyn Bytes) -> Result<Boxes, Bound> {
        if self.payload_len() >= 8 {
            let h = src.read(self.payload, 8)?;
            let size = u32::from_be_bytes([h[0], h[1], h[2], h[3]]);
            if size >= 8 && matches!(&h[4..8], b"hdlr" | b"keys" | b"ilst") {
                return self.children();
            }
        }
        self.children_after(4)
    }

    /// The version and flags of a FullBox.
    pub(crate) fn full(&self, src: &mut dyn Bytes) -> Result<(u8, u32), Bound> {
        if self.payload_len() < 4 {
            return Err(Bound);
        }
        let v = src.read(self.payload, 4)?;
        Ok((v[0], u32::from_be_bytes([0, v[1], v[2], v[3]])))
    }
}

/// The boxes in `pos..end` of a source, one header at a time.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Boxes {
    pos: u64,
    end: u64,
    depth: u32,
}

impl Boxes {
    /// The top-level boxes of a source of `len` bytes.
    pub(crate) fn top(len: u64) -> Self {
        Self {
            pos: 0,
            end: len,
            depth: 0,
        }
    }

    /// The top-level boxes from `pos` on (after a signature the caller has checked).
    pub(crate) fn top_from(pos: u64, len: u64) -> Self {
        Self {
            pos: pos.min(len),
            end: len,
            depth: 0,
        }
    }

    /// The next box's header, or `None` at the end of the container.
    pub(crate) fn next(&mut self, src: &mut dyn Bytes) -> Result<Option<BoxHeader>, Bound> {
        if self.end - self.pos < 8 {
            self.pos = self.end;
            return Ok(None);
        }
        let start = self.pos;
        let h = src.read(start, 8)?;
        let typ = [h[4], h[5], h[6], h[7]];
        if !SKIPPED.contains(&typ) {
            src.count_header()?;
        }
        let (size, mut header) = match u32::from_be_bytes([h[0], h[1], h[2], h[3]]) {
            // To the end of the container.
            0 => (self.end - start, 8),
            // A 64-bit size follows the type.
            1 => {
                let l = src.read(start + 8, 8)?;
                let large = u64::from_be_bytes([l[0], l[1], l[2], l[3], l[4], l[5], l[6], l[7]]);
                (large, 16)
            }
            n => (u64::from(n), 8),
        };
        let mut uuid = None;
        if typ == *b"uuid" {
            let u = src.read(start + header, 16)?;
            let mut id = [0; 16];
            id.copy_from_slice(&u);
            uuid = Some(id);
            header += 16;
        }
        // A size below its own header (this also refuses 2..7: the walk always moves on).
        if size < header {
            return Err(Bound);
        }
        let end = start.checked_add(size).ok_or(Bound)?.min(self.end);
        let payload = start + header;
        if payload > end {
            // The header itself is cut by the container's end.
            return Err(Bound);
        }
        self.pos = end;
        Ok(Some(BoxHeader {
            typ,
            uuid,
            start,
            payload,
            end,
            depth: self.depth,
        }))
    }
}

/// Whether an EXIF payload (HEIF's Exif item, JPEG XL's `Exif` box) holds a location: a 32-bit
/// big-endian OFFSET from the end of that field to the TIFF header (Apple writes 6 and then
/// `Exif\0\0`: critic F18), then the TIFF blob up to the payload's end. An offset of 0 followed by
/// `Exif\0\0` is read too (some writers forget the offset).
pub(crate) fn exif_payload(src: &mut dyn Bytes) -> Result<bool, Bound> {
    let len = src.len();
    if len < 4 {
        return Err(Bound);
    }
    let o = src.read(0, 4)?;
    let mut at = 4u64
        .checked_add(u64::from(u32::from_be_bytes([o[0], o[1], o[2], o[3]])))
        .ok_or(Bound)?;
    if at > len {
        return Err(Bound);
    }
    if len - at >= 6 && src.read(at, 6)? == *b"Exif\0\0" {
        at += 6;
    }
    let mut blob = Window {
        inner: src,
        base: at,
        len: len - at,
    };
    tiff::gps_in(&mut blob)
}

#[cfg(test)]
mod tests;
